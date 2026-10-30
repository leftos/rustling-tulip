# SSH remote: clients over OpenSSH without Tailscale

Linear: RT-530 (Connect remote clients over OpenSSH without Tailscale, with a simple pairing step), sub-issues SSH.1 to SSH.5. The release vNext holds only this and the work already started.

**Owner ruling (2026-10-29):** prioritize remote clients over OpenSSH (without Tailscale), including a straightforward pairing process. This reverses the non-goal "attach from outside the tailnet by SSH tunnelling".

## Decisions

**Orchestrator decisions (not owner rulings; open to the owner):**

1. **Transport.** The client runs `ssh -T -o BatchMode=yes -- <host> <remote command>`, where the remote command defaults to `rustling-tulipd bridge`. It speaks one JSON message per line over the child's stdio.
   - **The bridge, on the host:** runs `daemon_client::ensure_running`, opens the loopback `/ws`, and fills the `Hello` token from `daemon.json` itself, so the token never reaches the client.
   - **On exit:** the bridge exits on stdin EOF, on a stdout write error and on a closed WebSocket.
   - **Port-forwarding is rejected.** The daemon's port changes on every start, a stopped daemon cannot be started, and it would need `AllowTcpForwarding`.
2. **Trust.**
   - **SSH login is the owner:** the bridge's connection is an ordinary loopback (`Local`) connection to the daemon, so nothing changes on the protocol or the daemon side.
   - **Pairing:** it is the Add-host dialog's **Test connection** step. It runs the bridge once and shows the host's `Welcome`, or the `ssh` error, verbatim.
   - **Revoking:** a client is revoked by removing its SSH key on the host. An SSH client has no Devices row.
3. **The host string.** It is an `~/.ssh/config` alias or `user@host[:port]`, always passed after `--`. One that starts with `-` is refused, since `-oProxyCommand=…` would run code.
4. **Prompts.** `BatchMode=yes` means no prompt is ever shown. An unknown host key, a key with a passphrase and no agent, or a denied login fails the connection with the `ssh` stderr shown in the dialog. There is no `StrictHostKeyChecking=accept-new`.
5. **The remote command default** is `rustling-tulipd bridge`. The installer adds its folder to the user PATH (and uninstall removes it) through `tools/installer/user-path.ps1`, since the install folder is not on PATH otherwise. A saved SSH host may carry its own `remote_command`.
6. **v1 hosts are Windows only.** Linux and macOS hosts are not claimed. **Owner ruling (2026-10-30):** Linux and macOS hosts are planned for vNext+1, as RT-536.
7. **Saved hosts.** `saved_hosts` and `last_target` in `native-ui.json` gain `kind` (`"tailnet"` default, or `"ssh"`). An SSH entry stores its host string and an optional `remote_command`, and no port. Aliases are typed, not listed from `~/.ssh/config`.
8. **Device approval stays local.** The pairing and device-approval UI (`ListPendingDevices`, the pending-device prompts) shows only on `Target::Local`, never on an SSH or tailnet target. The daemon sends `PendingDevices` to every loopback connection, so the client must ignore it there.
9. **Accepted as they are:** the daemon's idle exit on an SSH-only host, and the unbounded per-connection output queue (RT-466, in vNext+1).
10. **No feature branch.** Each step lands on `main`. SSH.1 adds a subcommand nothing calls yet, and SSH.3's target stays unreachable until SSH.4's picker offers it.

## Steps

- [x] **SSH.1 `rustling-tulipd bridge`.** Files: `crates/daemon/src/cli_bridge.rs` (new), `crates/daemon/src/main.rs`. Proof: daemon tests against a fake loopback daemon, covering relay in both directions, the token put into `Hello`, exit on stdin EOF, no stdout output besides frames, and starting a daemon when none runs.
- [x] **SSH.2 Windows stdio check.** The bridge spawned through `cmd /c` with piped stdio, as sshd runs an exec command, returns its `Welcome` and exits with stdout at EOF, so the GUI-subsystem exe keeps its stdio and a daemon it starts holds none of its pipes. The installer puts its folder on the user PATH (decision 5). A real sshd session stays a hand check in `docs/remote-hand-checklist.md`: the command resolves on its PATH, and the daemon the bridge starts survives the session's end.
- [x] **SSH.3 `Target::Ssh` and the transport.** Files: `apps/native/src/target.rs`, `net.rs` (the socket becomes a boxed stream), `ssh.rs` (new), `remote_mode.rs`, `file_fetch.rs`, `lib.rs`, and the UI state in `sidebar.rs`. A hotspot. Proof: `net.rs` tests with a scripted child, capabilities, the folder name for fetched files, and the `ListPendingDevices` gate. Depends on SSH.1.
- [ ] **SSH.4 The picker's Add SSH host and Test connection.** Files: `apps/native/src/connection_picker*.rs`, `sidebar.rs` (`kind`), `tests/ui/ui_connections.rs`. Proof: UI specs and a `native-shot` of the picker. Depends on SSH.3.
- [ ] **SSH.5 Live e2e with a fake `ssh`.** Files: `tools/e2e/fake-ssh/` (selected by `RUSTLING_TULIP_SSH`, which checks the argv shape and runs the bridge locally), `apps/native/tests/support/live.rs`, `apps/native/tests/e2e_ssh.rs` (in the `serial-live` group), `.config/nextest.toml`, `rt.ps1`. Proof: `.\rt.ps1 native-e2e`. Depends on SSH.3.

Each step's docs sweep updates `docs/plans/remote-access.md`, `docs/architecture.md`, `CLAUDE.md`, `docs/native-client.md`, `docs/remote-hand-checklist.md`, `CHANGELOG.md` and `README.md` as its change reaches them.

## Map

The explorer's map follows, as read from main at `d3fc416b`. Its open decisions are settled above.

PROTOCOL: 1 overlay: none (docs/agents/explore.md absent), read CLAUDE.md; 2 README: skipped (CLAUDE.md carries the doc list; docs/README.md not opened); 3 Task Index: no row for SSH or any second remote transport (nearest: the tailnet-listener row, architecture.md:204); 4 design docs: done (docs/plans/remote-access.md whole; native-client.md searched for SSH/FetchFile only); 5 source: done; 6 tree search: done for target/net/handshake/daemon-client sites; Linear RT-530 itself not read (no fetch made); CI config not found.

# RT-530: remote clients over OpenSSH (design map)

## Answer in one paragraph

Build an `rustling-tulipd bridge` subcommand and a `Target::Ssh` in the native client. The client spawns `ssh -T -o BatchMode=yes -- <host> rustling-tulipd bridge` and speaks the existing protocol over its stdio. On the host the bridge runs `daemon_client::ensure_running`, proves and opens the loopback `/ws`, and puts the `daemon.json` token into the `Hello` itself. Trust is option (a): the SSH login is the owner, so there is no pairing, no new auth kind and no protocol change. The "pairing step" is the Add-host dialog's Test connection. The cost is no per-client revoke, which is cosmetic anyway (see Q2). The daemon needs no change except the new subcommand. Most of the work is in the client: a new target variant (about 39 sites in 14 files use `tailnet_host()`, `Target::Tailnet`, `is_local()`, `manages_daemon` or `TailnetHost`; count from this run's rg of non-test `apps/native/src`), a generic socket, and the picker.

## Facts from the code

- Loopback `/ws` is `AuthKind::Local` (`crates/daemon/src/server.rs:544-546`). It compares the `daemon.json` token constant-time (`handshake.rs:154-159`). `daemon.json` is written owner-only (`server.rs:538`, `secret::write_private`).
- `AuthKind::{Local, Tailnet(PeerIdentity)}` (`server/connection.rs:49-52`) is the only auth split. Sites that branch on it:
  - `handshake.rs:67,154,161` (token check);
  - `connection.rs:120` (pairing loop for tailnet only);
  - `connection.rs:206` (state forwarder `local`, which also sends the waiting pairing requests);
  - `remote.rs:451-466` (`host_only_refusal`: Shutdown, ConfigureRemote, the device-approval set; `Local` returns None);
  - `pairing.rs:550` (approvals);
  - `remote.rs:302` (tailnet router builds `Tailnet`).
- `FetchFile` dispatch (`server.rs:702`) has no auth gate, so file fetch works on a `Local` connection. The transport is just the WS.
- `Hello.auth_token` on loopback is the `daemon.json` token (CLAUDE.md "Wire-protocol gotchas"). The device store is keyed by the tailnet node `stable_id` (`devices.rs:57,194,230`), which an SSH client does not have.
- Native client, `apps/native/src/net.rs`:
  - `Target::{Local, Tailnet}` is matched at `net.rs:455-458`.
  - `attempt_local` (`:463-510`) runs `deps.ensure`, `open_local` with the `/proof` check, then `client_async` over `MaybeTlsStream::Plain(TcpStream)`.
  - `attempt_tailnet` (`:517-545`) runs `connect_async_tls_with_config`.
  - `type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>` (`:32`), 25 mentions in net.rs. `pump(ws: Socket, token)` (`:615`) is shared by both paths, so a third transport needs `Socket` made generic or boxed.
  - Failure, backoff and reconnect are `closed()` → `Next::Wait`, already transport-agnostic (`:425-435`).
- `apps/native/src/target.rs:9-17`: `Target` has two variants. `TailnetHost{name,port}` has `url()` = `wss://name:port/ws`. `parse_tailnet` refuses a bare label, IP literals, ports and userinfo (`:102-137`), so an ssh alias like `devbox` or `user@host` cannot pass it.
- `remote_mode.rs`: `Capabilities::for_target` is `target.is_local()` for both `local_files` and `daemon_management`. A new `Ssh` variant is non-local by default, so Stop, Reveal and the handshake row are already hidden.
- `file_fetch.rs:124-131`: `host_folder` uses `target.tailnet_host()`, so SSH needs a name there or fetch silently has no folder.
- Device tokens: `daemon-client/src/device_tokens.rs`, user `device/<lower host>`, Credential Manager. Not needed for SSH under (a).
- `cli_pair.rs` is already "the command-line way for an owner reached over SSH" (architecture.md:130). The same pattern (a subcommand that reads `daemon.json`, no instance lock) is the template for `bridge`. Its `PairSocket` (`cli_pair.rs:29`) is a ready loopback WS client. `main.rs:90-107` is where subcommands dispatch, before the instance lock.
- `daemon_client::ensure_running(client_versions)` (`supervisor.rs:143`) works on the host if called by the bridge process there. It resolves the daemon binary (`locate_daemon_binary`, `cache_daemon_binary`) relative to the running exe. Called from `bridge` it is the daemon's own exe, whose crate already depends on daemon-client (`crates/daemon/Cargo.toml:30`). Not run over SSH in this task, so unmeasured.
- Idle exit (`idle_exit.rs:1-40`): a connected WS client keeps the daemon up, and remote access on keeps it up regardless. A pure-SSH host with nobody connected and no live sessions exits after 30 s, and the next bridge's `ensure_running` restarts it. Sessions in tracers survive.
- The daemon is a GUI-subsystem exe on Windows (`cli_pair.rs:44-58`). Under sshd with an exec command, std handles are pipes inherited from sshd. Whether they survive for a GUI-subsystem exe is unproven here and must be a Windows proof step (SSH.2).
- Non-Windows host: `crates/daemon` and `crates/tracer` use `portable-pty`, `interprocess` with Unix sockets, and about 30 files with `cfg(windows)`/`cfg(unix)` branches. Windows-only dependencies: Credential Manager keyring, `windows` crate, ConPTY paths. There is no `.github/` directory and I saw no CI proving a Linux or macOS build. Whether the daemon builds there is unmeasured; do not promise it.

## Q1. Transport

| | stdio bridge (`ssh host rustling-tulipd bridge`) | `ssh -N -L` to loopback |
|---|---|---|
| ProxyJump and `~/.ssh/config` aliases | Free; the client only passes the host string to `ssh`. | Same. |
| Windows OpenSSH (`C:\Windows\System32\OpenSSH\ssh.exe`) and agent | Spawn `ssh.exe` (PATH, else that path, with `CREATE_NO_WINDOW`). It uses the user's agent, keys and config. `BatchMode=yes` means no prompts from a GUI app, so key or agent auth only in v1. | Same, plus managing a long-lived `-N` child and waiting for the forward to come up. |
| Daemon not running on the host | The bridge calls `ensure_running` on the host, so it self-starts. | The remote port is random per start (`daemon.json`), so it needs a prior `ssh host cat daemon.json` (remote-shell and OS dependent) and cannot start the daemon. Fails when the daemon has idle-exited, and the port changes on every restart. |
| Token | Never leaves the host (the bridge fills `Hello`). | The token must be fetched to the client and also sits behind a client-local port. |
| Local exposure | None; no listening socket anywhere. | Opens a client-loopback port reachable by any local user of the client PC. They still need the token, but it crosses the wire. Needs `AllowTcpForwarding` on sshd. |
| Reconnect | The child exits, `closed()` runs, the backoff calls `ssh` again. The bridge re-resolves the port every time. | A forward may linger or fail to rebind. |
| Large PTY output | Output is base64 JSON on every transport. Add `-o Compression=no`. The per-connection daemon queue is unbounded (`connection.rs:91`), so a slow link grows daemon memory. This is the same as tailnet today. | Same. |
| Remote file fetch | Plain WS `FetchFile` (already works on `Local`). | Same. |
| Host OS | The bridge is OS-neutral code but needs a daemon on the host (see non-Windows note above). | Same. |

**Recommendation: the stdio bridge.** Choose line framing over raw WS bytes.

- **Wire on stdio:** one compact JSON message per line, in each direction. This is the existing protocol with no WS framing.
- **Bridge:** it terminates the WS to loopback (reuse `cli_pair.rs`'s connect plus the proof from `daemon-client/src/proof.rs`). It replaces `auth_token` in the first `Hello` line with the host's token and relays the rest. It exits on stdin EOF, stdout error or WS close. Nothing but protocol lines goes to stdout, and logs go to the daemon log or stderr.
- **Why not raw WS bytes:** the client would need the token, so it would first have to read `daemon.json` over SSH.
- **Client side:** make `Socket` a boxed `Stream<Item = Result<Message>> + Sink<Message>` (or generic). A small adapter turns child stdout/stdin lines into `Message::Text`. `pump` and the rest of net.rs stay unchanged, and so does the tailnet path.
- **SSH argument safety:** the host string is argv only and must go after `--`. Refuse a host that starts with `-` (otherwise `-oProxyCommand=...` is code execution) or contains whitespace or control characters. The remote command is a fixed string, never built from user input.
- **ssh options:** `-T -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o ConnectTimeout=10 -o Compression=no`, and `kill_on_drop` on the child.
- **Host key and passphrase:** unknown host key or passphrase-protected key with no agent fails under BatchMode. Show ssh's stderr text and the hint "run `ssh <host>` once in a terminal". See OPEN DECISIONS.

## Q2. Pairing and trust

What SSH proves: a login as the host account over a key, a password or the agent. That account owns `daemon.json` (owner-only), so it can already read the token, spawn a shell and kill the daemon. No capability is added by letting it speak the protocol.

- **(a) SSH login is the owner, bridge uses the `daemon.json` token.** No protocol, store or daemon-auth change.
  - Revoke: remove the key from `authorized_keys`, or stop sshd. There is no Devices row, so `ForgetDevice` and `RevokeDevice` do not apply.
  - On the daemon it is a normal `Local` loopback connection.
  - Recommended. The pairing step is the Add-host dialog's Test connection, which runs the bridge with `--check` (or does the real connect) and shows ssh's text.
- **(b) Mint a device token over the SSH channel on first connect, stored in Credential Manager.** Needs a third `AuthKind` (or a synthetic `stable_id`), since `DeviceStore` keys on the tailnet node id (`devices.rs:194,230`). It also needs a bridge that connects with `Local` just to mint (the bridge already holds the owner token) and a branch in the handshake. The result is a revoke button in Devices that is cosmetic: a revoked SSH client can still log in, run the bridge and read the token (or run `rustling-tulipd pair`). The real credential is the SSH key. Not worth the surface for "smallest solid".
- **(c) Typed pairing code.** Works today only for the tailnet flow (`DevicePairRequest` needs a `PeerIdentity`) and costs a second terminal step. Rejected.

If the owner later wants per-client revoke, the least bad path is `command="rustling-tulipd bridge"` restricted keys in `authorized_keys`, documented rather than built.

## Q3. Host picker

Today `saved_hosts` is `[{name, port}]` and `last_target` is `{name, port}` or absent for this PC (`connection_picker.rs:18-21`, `sidebar.rs` UiState, CLAUDE.md native-ui.json bullet). A name that fails the DNS check is dropped on load.

- Add a `kind` field to both, `"tailnet"` (default when missing) or `"ssh"`. A saved SSH host is `{kind: "ssh", name: "<alias or user@host>", remote_command: <optional>}` with no port. An unknown kind is dropped on load, like a bad name.
- `Target::Ssh(SshHost)` with `name()` (the exact string handed to ssh), `short_name()` (the alias, or the part after `@`) and `address()` for display.
- Generalise the sites that use `tailnet_host()` to a `remote_name()`. Lists of sites: `connection_picker.rs:45,60,291,310`, `connection_picker_view.rs:388`, `device_pairing_view.rs:221`, `file_fetch.rs:130`, `layout_chooser_view.rs:282`, `target.rs:168-287`. Also `host_session_counts` is keyed by lower-cased MagicDNS name, so SSH keys must not collide (prefix `ssh:`).
- Picker row: a distinct icon or "SSH" tag, label `devbox (ssh)`, with "Add SSH host…" next to the tailnet typed entry. Validation lives in a new `parse_ssh`: no leading `-`, no whitespace, non-empty.
- Optionally list `Host` aliases from `~/.ssh/config` (skip wildcards). This is a convenience, not required; see OPEN DECISIONS.
- Forget removes only the saved entry. There is no device token to delete for SSH (the tailnet path deletes the stored token).
- Capabilities: `Ssh` is non-local, so local-file actions and Stop are off, as for a tailnet host.

## Q4. What a remote session can do (invariant pass)

**Actors**

1. **The owner's client over SSH.**
   - The daemon sees a `Local` connection, so all messages are accepted, including Shutdown, ConfigureRemote and the approval set. That is the owner anyway.
   - UX policy (invariant 9) stays client-side: Stop and Restart are hidden by `Capabilities`, quit only disconnects.
   - The client must not call `ListPendingDevices` on SSH, and the pairing badge must not show: that call is currently skipped because "never on a tailnet host" (remote-access.md:16) and runs on every Local connection to the PC's own daemon. Gate it on `Target::Local`, not on "not tailnet".
   - For a Local connection the state forwarder also pushes `PendingDevices` unprompted (`connection.rs:206`). The client should ignore them when the target is not Local, or the Settings section would show the host's waiting devices on an SSH host. That is acceptable (the owner may approve from there), but then the approval dialog works remotely too. Decide (OPEN DECISIONS 5).
2. **Another local user on the host.**
   - Can reach the loopback port but not `daemon.json` (owner-only). Unchanged.
   - The bridge adds no listener, so SSH adds no new reachable surface.
   - A different host user running `rustling-tulipd bridge` starts or uses their own daemon in their own config dir.
3. **A bridge outliving its client.**
   - If ssh dies, sshd closes the channel, the bridge sees stdin EOF and exits, and the daemon sees a WS close. A hung TCP link is bounded by `ServerAliveInterval` (client side) and the client's existing resume probe and welcome watchdogs.
   - A bridge must exit on stdin EOF, on any stdout write error and on a WS close. Add a test that kills the parent and asserts exit.
   - The bridge holds the token only in memory and never prints it.
4. **An attacker without SSH access.** Nothing new is reachable. The tailnet listener's `Origin`/`Host` rules do not apply: there is no browser-reachable endpoint.
5. **A malicious `~/.ssh/config` host or user-supplied host string.** The option-injection check above, plus a fixed remote command. A malicious *remote host* only controls the stream it sends: the client decodes the protocol it already decodes from tailnet hosts.

**Sites that decide "loopback" or "tailnet" and must decide for SSH**

Daemon (all unchanged by design, since SSH arrives as `Local`):
- `server.rs:544-546` `ws_handler`
- `handshake.rs:67,154,161`
- `connection.rs:120,206`
- `remote.rs:451` `host_only_refusal`
- `pairing.rs:550`

Client (these change):
- `net.rs:455-458` `attempt`, a third arm.
- `net.rs:749` `let Target::Tailnet(host) = ...` (pairing/token path, `SSH` has none).
- `remote_mode.rs` (`for_target`) and `Target::is_local` callers.
- The `ListPendingDevices` gate above.
- `file_fetch.rs:124-131` `host_folder`.
- The `device_tokens` lookup (`net.rs:549`), skipped for SSH.
- Any "tailnet" wording in `device_pairing_view.rs`, `layout_chooser_view.rs` and the quit dialog.

Invariants to restate for SSH in `remote-access.md` (credential never goes to another host holds: the SSH client never holds the token; "remote client is full control" holds).

**Remote file fetch** works unchanged (`FetchFile` is ungated; the fetched-file folder comes from `host_folder`).
**`connected` counts** (`DeviceInfo.connected`) only count tailnet devices (`live_devices.rs:10` says loopback connections are never registered). SSH connections will not show in Devices. That is correct for (a) but should be said in the docs.

## Q5. Tests

By analogy with fake-tailscale (`tools/e2e/fake-tailscale/`, env `RUSTLING_TULIP_TAILSCALE`):

- Add `tools/e2e/fake-ssh/` (`fake-ssh.cmd`, `.sh`, `index.mjs`; a tiny shell shim, node-free if possible) selected by `RUSTLING_TULIP_SSH` (default `ssh`, then the Windows OpenSSH path).
- The shim asserts the argv shape (`-T`, `BatchMode`, `--`, the host) and records it to a file. It then execs `rustling-tulipd bridge` locally with `RUSTLING_TULIP_CONFIG_DIR` of the test daemon, ignoring the host. A failing mode (a state-file flag) exits 255 with "Permission denied" on stderr to test the error text.
- No daemon feature flag is needed: a bridge to a real loopback daemon is the production path. This is simpler than `e2e-loopback-peers`.
- Wire it where `LiveDaemon` sets `RUSTLING_TULIP_TAILSCALE` (`apps/native/tests/support/live.rs`), and add `apps/native/tests/e2e_ssh.rs`, a new serial-live member (`.config/nextest.toml`). Specs: connects and lists sessions, spawn plus PTY echo, `FetchFile` writes under `remote-files/<ssh host>/...`, killed ssh reconnects, bridge exits on stdin close, daemon not running at start is started by the bridge, option-injection hosts refused.
- Unit: `bridge` line relay and `Hello` token substitution with a fake loopback (daemon crate); the line adapter and `parse_ssh` (native); a net.rs test with a scripted child like the existing `run_net` tests (`net.rs:1507`).
- UI specs: picker rows and Add-SSH-host dialog (`tests/ui/ui_connections.rs`); capabilities for `Ssh` in `ui_remote.rs`.
- Hand checklist addition (`docs/remote-hand-checklist.md`): real Windows OpenSSH to a Windows host with ProxyJump, agent and the GUI-subsystem stdio check.

## Files and anchors per part

Daemon (`bridge`):
- `crates/daemon/src/main.rs:80-107` dispatch (new `cli_bridge::requested`), before the instance lock, like `pair`.
- New `crates/daemon/src/cli_bridge.rs`. Model on `cli_pair.rs` and the proof code in `crates/daemon-client/src/proof.rs`, and call `daemon_client::ensure_running`.
- Make sure no log writes go to stdout.

Client:
- `apps/native/src/target.rs` (variant, `SshHost`, `parse_ssh`).
- `apps/native/src/net.rs` (`attempt_ssh`, generic socket, child spawn, `NetDeps` gains an ssh spawner seam like `open_local`).
- `apps/native/src/ssh.rs` (new: locate ssh, argv, line adapter).
- `apps/native/src/connection_picker.rs`, `connection_picker_view.rs`.
- `apps/native/src/sidebar.rs` (`UiState` saved hosts and `last_target` with `kind`).
- `remote_mode.rs`, `file_fetch.rs`, `lib.rs` (the `ListPendingDevices` gate and boot target), `device_pairing_view.rs`, `layout_chooser_view.rs`.
- `tools/test-map.json` rows for new files.

Tests: `tools/e2e/fake-ssh/`, `apps/native/tests/support/live.rs`, `apps/native/tests/e2e_ssh.rs`, `apps/native/tests/ui/*`, `.config/nextest.toml`, `rt.ps1` (`native-e2e`).

## Protocol change or bump

None. No new message, field or feature string; the bridge speaks the current protocol with the current `Hello`. A `Welcome.features` entry is not needed. `protocol-version.json` stays `{25, [25]}`. No daemon-handshake behaviour changes. (A `bridge` subcommand is not wire-visible.)

## Proposed split

- **SSH.1: `rustling-tulipd bridge`** (daemon only, non-hotspot).
  - Files: `cli_bridge.rs`, `main.rs`.
  - Proof: daemon tests with a fake loopback daemon: relay both ways, token substituted into `Hello`, exit on stdin EOF, no stdout noise, starts a daemon when none runs.
  - Depends on: none.
- **SSH.2: Windows stdio proof** (hand check plus a small scripted test).
  - Proof: `ssh localhost rustling-tulipd bridge` on Windows OpenSSH returns a `Welcome` line (the GUI-subsystem exe keeps its stdio), and the install-dir command resolves.
  - Result decides the remote-command default (OPEN DECISIONS 2).
  - Depends on: SSH.1.
- **SSH.3: `Target::Ssh` and net** (hotspot: `net.rs`, `lib.rs`; central, so full suite).
  - Files: `target.rs`, `net.rs`, new `ssh.rs`, `remote_mode.rs`, `file_fetch.rs`, `lib.rs`, UI-state in `sidebar.rs`.
  - Proof: net.rs tests with a scripted child, capabilities, folder name, the `ListPendingDevices` gate.
  - Depends on: SSH.1.
- **SSH.4: picker and Add-host UX**.
  - Files: `connection_picker*.rs`, `sidebar.rs` (`kind`), `tests/ui/ui_connections.rs`; Test connection.
  - Proof: UI specs, `native-shot` of the picker.
  - Depends on: SSH.3.
- **SSH.5: live e2e with fake-ssh**.
  - Files: `tools/e2e/fake-ssh/`, `live.rs`, `e2e_ssh.rs`, `nextest.toml`, `rt.ps1`.
  - Proof: `.\rt.ps1 native-e2e`.
  - Depends on: SSH.3.
- **SSH.6: docs pass** (can go with each item, final tick last): `docs/plans/remote-access.md`, `docs/architecture.md`, `CLAUDE.md`, `docs/native-client.md`, `docs/remote-hand-checklist.md`, `CHANGELOG.md`, `README.md`.

SSH.1 and SSH.3 can overlap only in review: they touch disjoint files.

## Docs to update

- `docs/plans/remote-access.md`: title and intro ("Tailscale-first" becomes "two ways in"), add an SSH section with the invariants (SSH arrives as `Local`, bridge holds the token, no listener, fixed remote command, option-injection rule), retire "attach from outside the tailnet" and the P6 item list gets SSH.n.
- `docs/architecture.md:289` non-goal: delete the "SSH tunnelling" part ("internet exposure or a relay" stay non-goals). Add a "Remote (SSH) access" bullet near `:118-144` and a Task Index row. Also line 23 ("The daemon binds only loopback and ... tailnet") is still true but add the bridge.
- `CLAUDE.md`: the last bullet ("Things that are deferred") and the project-shape line for `crates/tailnet`; the `rustling-tulipd bridge` command beside `pair`/`shutdown`; the env var table (`RUSTLING_TULIP_SSH`); the `native-ui.json` bullet (`saved_hosts`/`last_target` `kind`); the E2E tier bullet (fake-ssh).
- `docs/native-client.md` (picker, fetched files host folder), `docs/remote-hand-checklist.md`, `CHANGELOG.md` (Unreleased bullet), `README.md` (feature showcase at release).

## DEPENDS ON

- None found. Not checked: open Linear items for the same files. `apps/native/src/lib.rs` and `net.rs` are hotspots, so SSH.3 must wait for any other open item that edits them (check `docs/plans/MAIN.md` before dispatch). The Linear issue RT-530 was not read in this run.
- SSH.2 (Windows GUI-subsystem stdio and the install-dir command) gates the remote-command default.

## BRANCH

`feat/ssh-remote`, no existing branch of that name (`git branch -a --list "*ssh*" "*rt-530*"` returned nothing). Base: main at `d3fc416b`. It is the release's only new feature, so one feature branch with SSH.1 to SSH.6 as stacked commits is reasonable.

## OPEN DECISIONS (ranked)

1. **Trust model (a) vs (b).** Recommend (a): no revoke row, because the SSH key is the real credential. The owner should confirm "no per-client revoke" is acceptable. If not, restricted `authorized_keys` command keys, documented.
2. **Remote command default.** The installer puts the daemon in `%LOCALAPPDATA%\Programs\rustling-tulip` (`tools/installer`, line 43), not on PATH, and a Windows sshd session starts in the user profile with the system PATH. Options: add the install dir to the user PATH in the installer; or default to a full-path command per host in the saved entry (`remote_command`); or both. Recommend a per-host optional field plus the installer adding PATH. Needs SSH.2.
3. **Host OS.** Support only Windows hosts in v1, or Linux/macOS too. The daemon's non-Windows build is unmeasured and there is no CI for it. Recommend Windows host only, stated in docs, and a separate issue for the port.
4. **Host-key and passphrase handling in a GUI app.** BatchMode (key or agent only; host key must already be known, with a "run `ssh host` once" hint) vs `StrictHostKeyChecking=accept-new` vs askpass. Recommend BatchMode plus a clear error, no `accept-new` (silent first-contact trust is a security decision for the owner).
5. **Approval and pending-device UI over SSH.** Since the daemon sees `Local`, the host's waiting devices and the Devices section could work remotely. Recommend: hide them on SSH (gate on `Target::Local`) and note in docs.
6. **Framing: JSON lines (recommended) vs raw WS over stdio.** Lines need the bridge to terminate WS and edit `Hello`; raw WS needs the client to learn the token. Recommended lines.
7. **`~/.ssh/config` alias listing** in the picker (optional, convenience).
8. **Idle exit with no clients on an SSH-only host:** the daemon exits after 30 s with no sessions (tracer sessions survive). Accept (bridge restarts it) or hold the daemon while any SSH host is configured. Recommend accept.
9. **Backpressure:** the unbounded per-connection output queue (`connection.rs:91`) on a slow SSH link. Same exposure as tailnet today; out of scope unless measured.

## DOC DRIFT

- `docs/architecture.md:289` and `CLAUDE.md:383` say SSH tunnelling is a non-goal and the tailnet listener is the one way in; the ruling (2026-10-29) reverses it. Docs are wrong after this feature lands: owner `docs/architecture.md` Non-goals, `CLAUDE.md`, `docs/plans/remote-access.md` (intro "Tailscale is a requirement for remote use").
- `docs/architecture.md:130` calls `rustling-tulipd pair` "for an owner reached over SSH", which already presumes SSH access without a client path. Not wrong, but it links to a non-goal. Fix with the same pass.
- `docs/plans/remote-access.md:16` says the client asks `ListPendingDevices` "never on a tailnet host", while the code gate is Local-vs-not (Connection state). Not verified against `lib.rs` in this run; recheck when adding the SSH target.

## TASK INDEX GAP

The answer needed about 25 reads and searches beyond the Task Index row. Row that should exist:

"Add or change a way for a client to reach a daemon on another machine (a connection target such as tailnet or SSH)" → `docs/plans/remote-access.md` → `apps/native/src/target.rs` (`Target`) → `apps/native/src/net.rs` (`attempt`, `attempt_local`, `attempt_tailnet`, `pump`, `Socket`) → `apps/native/src/remote_mode.rs` (`Capabilities`) → `apps/native/src/connection_picker.rs`, `connection_picker_view.rs`, `sidebar.rs` (`saved_hosts`, `last_target`) → `crates/daemon/src/server/connection.rs` (`AuthKind`) → `server/handshake.rs` (`check_hello_auth`) → `server/remote.rs` (`host_only_refusal`) → `crates/daemon/src/main.rs` (subcommands: `cli_pair.rs`, `cli_shutdown.rs`) → `apps/native/tests/e2e_remote.rs`.

## MISSING ENTRY POINT

none (docs/architecture.md and its Task Index exist; the repo has no `docs/agents/explore.md` overlay, so CLAUDE.md was read in its place). `docs/README.md` was not opened, so a glossary gap, if any, is unmeasured.
