# Remote access over Tailscale

Design for Wave 3's remote desktop (Linear RT-33, RT-34 and their sub-issues). Remote use of the daemon is **Tailscale-first**: Tailscale is required, and the daemon's only remote listener serves its tailnet. Local use (loopback, the `daemon.json` token) is unchanged. The iPhone PWA (RT-143, `docs/research/pwa-iphone.md`) reaches the same listener.

## Owner rulings

- Tailscale is a requirement for remote use. The daemon serves TLS with its `tailscale cert` (`*.ts.net`, public CA), one listener for the desktop client and the PWA; clients verify the certificate normally and nothing is pinned.
- A connecting device is authorized by `tailscale whois` **plus a per-device token** stored on the device. No pairing codes. A device without a token (a new device, or the PWA after Safari evicts its storage) asks for one, and the token is issued only once the owner **approves that device on the host PC**: whois alone never mints a token, so another program or Windows account on an owner device cannot get one unseen.

  Approval is a **QR code** the host PC shows, which the device scans (the iPhone PWA with its camera), with a short **pairing code** beside it that a device with no camera types instead; the QR code and the code are one-time, short-lived and good only for a whois-authorized device. A wiped phone is approved again.

  The native client's views of that request live in `apps/native/src/pairing_view.rs` over the model in `pairing.rs`. Settings → Remote access shows a `Waiting devices · {n}` section (name and Tailscale login, a `m:ss left` countdown, Approve… and Deny; an approved row reads `Approved · waiting for the device` with only Deny), and the Remote access tab carries an amber count badge (`99+` cap, approved requests included).

  Approve… opens an approval dialog that sends nothing until Approve is pressed, then shows the QR code, the code as `735 160` and `expires in m:ss · this request only`, and closes by itself when the code runs out. Disconnecting or switching target closes it and forgets the waiting requests.

  On each connect to this PC's daemon whose `Welcome` lists `device_pairing` the client asks `ListPendingDevices` (never on a tailnet host), and the daemon's `PendingDevices` and `DevicePairCode` feed the rows, the badge and the dialog. Each new unapproved request raises a Windows toast, "A device asks for remote access", with Approve… and Deny (`notify.rs`, raised from `pairing_view.rs`): Approve… brings the window forward and opens the dialog without opening Settings, Deny sends `DenyDevice`.

  The device's side of the same request is the native client's "Pair with {host}" dialog (`device_pairing.rs`, `device_pairing_view.rs`): it sends `DevicePairRequest`, waits for the owner's approval on the host PC, takes the pairing code typed from the host and stores the returned `DeviceToken` for that host before reconnecting (`docs/native-client.md`, "Pair dialog").
- "The owner" is a device logged in as the host node's own Tailscale user (numeric `UserID`, never the display login), untagged, not shared in, and not the host itself.
- The listener binds the host's own tailnet addresses only (v4 and v6), on the port `remote.json` configures (default 8787), rebinding when they change.
- Shutdown and turning remote access off are refused from tailnet connections: host-only.
- Revocation is a block list in rustling-tulip: Settings lists the approved tailnet devices (each with its `last_seen` and a `connected` count), and blocking one drops its live connections at once and refuses it at every pairing path until the owner forgets it (RT-96). The daemon half landed (RT-393); the native model (RT-391) and the Settings section (RT-392) are open.
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
- Remote tests: in-process tests with a fake whois and a test CA, a fake-TLS daemon for the client, and a hand checklist on real Tailscale (two machines and the iPhone). No production setting widens the allowed address ranges.

## Invariants and the sites that obey them

1. **Only tailnet peers reach the remote listener.** Bind the tailnet addresses from `whoami`; the acceptor rejects any peer outside `100.64.0.0/10` or `fd7a:115c:a1e0::/48` (IPv4-mapped addresses canonicalised) before whois. The daemon never binds `0.0.0.0` (`Ipv4Addr::UNSPECIFIED`): its only other listener is loopback. Site: new `crates/daemon/src/tailnet_listener.rs`.
2. **whois is asked about the real peer and fails closed.** The argument is the accepted socket's `peer_addr()`, never a header or a `Hello` field. Timeout (2 to 3 s), non-zero exit, unparsable JSON, a missing binary and the Windows "server mode" 401 all reject. Verdicts are cached per IP for at most 30 s. Sites: the custom `axum_server::accept::Accept` in the listener module; `crates/tailnet/src/cli.rs`.
3. **The owner rule** is a pure function: allow only when whois succeeded, the peer node has no tags, its `UserID` equals the host node's, it is neither a sharer's nor a sharee node, its `ID` differs from the host's (this rejects other Windows users and anything proxied on the host), and the peer IP is one of the node's own `Addresses` (this rejects subnet-routed sources). A tagged host binds nothing and asks for no certificate; the status says `host_tagged`. Site: `crates/tailnet/src/rule.rs`.
4. **The listener decides how a connection authenticates.** Loopback connections compare the `daemon.json` token; tailnet connections never look at it, and they present their device token instead. The loopback listener serves `build_loopback_router`; the tailnet listener has its own `build_tailnet_router`, and `/shutdown` is never on it.

   No router serves `/pair`. Site: `server/connection.rs` `AuthKind`, `server/handshake.rs` `handshake`, `server/remote.rs` `tailnet_ws_handler` and `build_tailnet_router`, `server.rs` `ws_handler` and `build_loopback_router`.
5. **A credential never goes to another host.** The native client sends the `daemon.json` token only to the loopback daemon; on a tailnet target it sends its device token for that host, or an empty one when none is stored, and lands in "not paired" when the host has not approved the device. No status message carries a token. Site: `apps/native/src/net.rs` (`attempt`).
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
| RT-96 | Remote devices: list the approved tailnet devices, block one and drop its connections. Daemon half landed (RT-393); native model (RT-391) and Settings section (RT-392) open | yes | TS.4, P6.4 (landed) |
| P6.1 | Transport seam: `Target::{Local, Tailnet(host)}` in `net.rs` over public-CA TLS. Landed on main | no | TS.1 |
| P6.2a | Remote-mode rules: local-file actions disabled with a reason, quit only disconnects, Stop, handshake and `daemon.log` hidden, Reconnect label, the capability table. Landed on main (RT-151) | no | P6.1 |
| P6.2b | The typed host path in place of the pickers on a remote host. Landed on main (RT-257) | no | P6.2a |
| P6.3 | Host picker from the tailnet's machines, a typed-name fallback, boot reconnect | no | P6.1, P6.2a |
| P6.4 | Host Settings: Remote access (Tailscale) panel. Landed on main (RT-154) | no | TS.4 |
| P6.5 | Autostart: start the daemon on login | no | none |
| P6.6 | Remote tests over a fake Tailscale, the hand checklist, and the docs pass | no | all of the above |
| P6.7 | Cutover: installer (no firewall rule, per RT-157) | no | P6.5 |

File fetch (RT-35 to RT-37) follows P6.1 to P6.3; a host's download folder is keyed by its MagicDNS name, lower-cased.

### P6.3 parts (RT-350 to RT-354)

A1 is RT-350, A2a RT-351, A2b RT-352, B RT-353, C RT-354; the orchestrator decisions on RT-152 settle the open choices. Line numbers are at ec89ea15.

#### P6.3 part A1: DNS-name check (small, 1 file + docs). Landed on main
- Files: apps/native/src/target.rs (test-map row exists at tools/test-map.json:36, no change needed).
- Steps: add `HostNameError` (Display = the message the picker shows) and `Target::parse_tailnet(&str, Option<u16>)` (reuse by `Target::tailnet` is NOT wanted: keep that infallible for test fixtures); table tests.
- Red-first tests (target.rs `mod tests`): `parse_tailnet_accepts_a_magicdns_name`, `parse_tailnet_trims_one_trailing_dot_and_lowercases`, `parse_tailnet_refuses_userinfo`, `parse_tailnet_refuses_path_query_and_fragment`, `parse_tailnet_refuses_a_port`, `parse_tailnet_refuses_whitespace_and_percent`, `parse_tailnet_refuses_empty_and_empty_labels`, `parse_tailnet_refuses_bad_label_edges` (leading/trailing hyphen, 64-char label, 254-char name), `parse_tailnet_refuses_ip_literals`, `parse_tailnet_refuses_a_bare_label` (only if OPEN DECISION 5 rules so).
- Gate: `cargo nextest run -p rustling-tulip-native --cargo-profile test-release target::`; full suite last (stable mode).

#### P6.3 part A2a: specs load after the switch (test-only refactor, lands green before A2b). Landed on main
- Files (6): apps/native/tests/support/mod.rs (central in the test map: full suite), apps/native/tests/ui/ui_remote.rs, ui_remote_access.rs, ui_file_fetch.rs, ui_fetch_dialog.rs, plus tools/test-map.json only if a row needs a new key (none expected).
- Steps: add `Harness::on_target(cx, dir, target, &fixture)` (opens, reports `State` for `target` via `NetEvent::State(open_on(target, port))`, `Handshake`, `Welcome`, then `load(fixture)`) and a `Harness::switch_to(conn, &fixture)` for specs that start Local; migrate the 40+ sites listed in section 2.4 so each loads its fixture after the switch. Behaviour-neutral today, so it passes before and after A2b.
- Tests: no new names; the migrated specs must stay green. Red-first concern: none (refactor), prove it by running `ui_remote|ui_remote_access|ui_file_fetch|ui_fetch_dialog` before/after.
- Note `ui_remote.rs` is also in the `settings_view.rs`, `grid_view.rs` (concurrent item), `file_fetch_view.rs`, `quit_view.rs`, `connection.rs` rows' filters: no map edit.

#### P6.3 part A2b: the reset and the exit dialog (hotspot: lib.rs, central). Landed on main
- Files (6): apps/native/src/lib.rs, apps/native/src/sidebar.rs, apps/native/src/tabs.rs, apps/native/src/quit_view.rs, apps/native/tests/ui/ui_target_switch.rs (new) + apps/native/tests/ui/main.rs (`mod ui_target_switch;`, central) + tools/test-map.json (new spec's row; sidebar.rs/tabs.rs/quit_view.rs rows' filters gain `ui_target_switch`).

  That is 7 files counting main.rs; main.rs and test-map.json are one-line edits. If it must be 6, put the new spec inside ui_remote.rs (already a module) and skip main.rs and the new test-map row.
- Steps: (1) `SidebarModel::clear_daemon_state`, `TabsModel::clear_for_switch`; (2) `RootView::reset_for_target`, factoring the send-free part out of `on_welcome` (lib.rs:2554-2587) so both call it; (3) call it from the State arm (lib.rs:2235) before `self.conn = conn`; (4) close the exit dialog there (quit_view.rs:131/:187) and, for robustness, check `manages_daemon()` before `exit.is_some()` at quit_view.rs:131-136; (5) decide pruning/persistence of daemon-id keyed ui state (OPEN DECISION 3).
- Red-first tests (UI specs, names): `target_switch_clears_the_old_hosts_sessions_repos_and_tabs`, `remote_to_local_switch_with_ensure_error_leaves_no_clickable_rows` (use `Connection` in `State::Error` for Local after remote rows), `reveal_and_path_ctrl_click_are_gone_after_remote_to_local_error`, `target_switch_sends_nothing_to_the_daemon` (assert `h.sent()` is empty: no Detach, no GetFileSnapshot, no ListSessionHistory), `target_switch_does_not_save_native_ui_json` (read the file before/after: `active_tab_id` unchanged), `same_target_reconnect_keeps_the_rows`, `target_switch_clears_sc_history_diff_tabs_and_worktrees_root`, `target_switch_closes_the_exit_dialog` and `stop_keep_after_a_switch_to_remote_sends_no_shutdown` (put these two in ui_quit.rs only if the file budget allows; ui_quit.rs is in the quit_view.rs row), unit tests `clear_daemon_state_keeps_the_ui_state` (sidebar.rs), `clear_for_switch_empties_tabs_and_marks_unloaded` (tabs.rs).
- Gate: ui_target_switch|ui_remote|ui_remote_access|ui_quit|ui_file_fetch|ui_fetch_dialog|ui_tabs|ui_sidebar; lib.rs is central so the last gate is the full suite.

#### P6.3 part B: peers, saved hosts, boot target (model, no UI). Landed on main
- Files (6): apps/native/Cargo.toml (+ Cargo.lock, both central: full suite), apps/native/src/connection_picker.rs (new, plain Rust + tests), apps/native/src/sidebar.rs (`UiState` fields `last_target`, `saved_hosts`; accessors/setters; defaults :535; the round-trip literal near sidebar.rs:2992), apps/native/src/net.rs (central; `spawn`/`spawn_with` take the boot `Target`), apps/native/src/lib.rs (`pub mod connection_picker;`, read ui state first in `new`, `conn: Connection::for_target` at :891), tools/test-map.json (rows for connection_picker.rs, widen target.rs/sidebar.rs rows).

  7 with test-map.json; drop Cargo.lock from the count (generated).
- Red-first tests: connection_picker.rs: `rows_list_local_then_saved_then_online_peers`, `peer_rows_use_the_dns_name_without_the_trailing_dot`, `offline_tagged_and_shared_peers_are_not_offered` (per OPEN DECISION 4), `typed_name_goes_through_the_dns_check_and_names_the_refusal`, `forget_removes_a_saved_host`, `resolve_boot_target_uses_a_saved_host`, `missing_saved_host_falls_back_to_local` (ruling: saved name absent from the saved list falls back; unreachable host does not), `tailscale_errors_show_their_display_text`; sidebar.rs: `last_target_and_saved_hosts_round_trip`, `a_hand_edited_bad_saved_host_is_dropped_on_load` (goes through `parse_tailnet`); net.rs: `net_starts_on_the_boot_target` (no `ensure_running` call for a tailnet boot target; fake `ensure` panics if called, in the style of net.rs tests at :1281); UI spec `boot_on_a_saved_host_starts_on_that_target` in a new or existing module (support/mod.rs seeds native-ui.json).
- Tailscale seam: `PeerSource` trait object with a system impl that runs `tailnet::Cli::locate().status()` on a one-shot `std::thread` + current-thread tokio runtime (needs a reactor; see OPEN DECISION 6).

#### P6.3 part C: the picker view. Landed on main

The typed-host entry has a Port field beside the name, prefilled with 8787; a saved host keeps the port it was added with, and a listed peer dials 8787 unless a saved host of that name records another (RT-152 decision, since the Remote access page lets a host change its port). Part B's saved-host model stores and round-trips that port.
- Files (6): apps/native/src/connection_picker_view.rs (new), apps/native/src/lib.rs (field + focus handle + layer in the chain + key routing + `footer_bar` Connections button at :3234 + keep the layer above the first-connect `overlay` at :3200, + `overlay_open` :1222), apps/native/tests/ui/ui_connections.rs (new), apps/native/tests/ui/main.rs (`mod ui_connections;`), apps/native/tests/support/mod.rs (peer-source recorder, switch accessor), tools/test-map.json.

  Add apps/native/examples/shot.rs only if a `connections` shot view is wanted (shot.rs:261 builds RootDeps; avoid by using the setter approach).
- Red-first UI specs: `connections_button_opens_the_picker`, `picker_lists_saved_hosts_and_peers`, `selecting_a_host_sends_switch_target` (assert via `h.commands()`, support/mod.rs:1265), `typed_name_with_a_slash_shows_the_message_and_sends_nothing`, `forget_removes_a_saved_host_and_the_row`, `picker_is_reachable_while_the_first_connect_overlay_shows`, `escape_closes_the_picker`, `switching_host_saves_last_target`, `picker_shows_tailscale_not_running_text`, `picking_this_pc_switches_to_local`.
- Also: docs pass (section DOC DRIFT), CLAUDE.md native-ui.json bullet (new keys), native-client.md picker paragraph, architecture.md Task Index row, native-client-parity.md:196 tick.

File sets: `apps/native/src/lib.rs` (RootView hotspot, central in the test map, so each edit runs the full suite) is touched by A2b, B and C, so they run one at a time. `apps/native/src/net.rs` only by B. `apps/native/tests/support/**` and `tests/ui/main.rs` by A2a, A2b and C. A2b clears the pane map in `lib.rs` and does not edit `grid_view.rs`.

## Facts relied on

From Tailscale's docs (kb/1080 CLI, kb/1153 enabling HTTPS, kb/1084 sharing, the tags and identity docs, kb/1033 addresses) and source (`apitype.go`, `tailcfg.go`, `ipn/ipnlocal/cert.go`), checked when this design was written: `whois --json` returns `{Node, UserProfile, CapMap}`, never nil on success; `Node.User` does not reflect a tagged node's identity; a shared node appears as its sharer; HTTPS certificates must be enabled in the admin console and are 90-day Let's Encrypt certificates that `tailscale cert` renews below `--min-validity`; machine names are published in CT logs; a node's IP is stable while it stays registered.

Not yet verified: whether `tailscale cert` runs unelevated for the Windows user who owns Tailscale, how fast a removed device's live TCP connection dies, and the exact IPv4 range text in Tailscale's docs.
