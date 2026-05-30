# Remote LAN Access — Control Desktop Shells from a Laptop

## Status (2026-05-29)

- [x] Dependency guardrail — `rustls`/`tokio-rustls`/`axum-server`/`rcgen` on the
      `ring` backend (no `aws-lc-sys`); verified no new `cargo deny` failures.
- [x] Phase 0 — persistent auth token (`lan.json`).
- [x] Phase 1 — opt-in TLS LAN listener + self-signed cert + fingerprint.
- [x] Phase 2 (LAN scope) — `ConfigureLan`/`LanStatus` protocol, runtime
      enable/disable of the listener, status broadcast + initial state, TS mirror.
      **Re-sequenced:** the `client_id` Hello field moved to Phase 7 (per-client
      layouts), where it is actually consumed.
- [x] `cargo deny check` made green (pre-existing failures fixed: app license,
      CDLA-Permissive-2.0, `allow-wildcard-paths` + `publish = false`,
      `unmaintained = "workspace"`).
- [x] Phase 3 — desktop UI: "Remote access" settings tab (enable toggle, port,
      address picker, copyable connection code).
- [x] Phase 4 — Tauri pinned-TLS loopback tunnel + remote-profile store
      (`apps/tauri-app/src-tauri/src/remote.rs`). **Deviation:** `connect_remote`
      takes resolved `ConnectionParams` (not the raw code) for uniformity across
      the paste flow and saved-profile reconnect; a separate
      `decode_connection_code(code)` does the pure decode, and `disconnect_remote`
      tears the bridge down. Commands: `connect_remote`, `disconnect_remote`,
      `decode_connection_code`, `list_remote_profiles`, `save_remote_profile`,
      `delete_remote_profile`. Pinned-verifier accept/reject + code/fingerprint
      parsing are unit-tested.
- [x] Phase 5 — frontend connection picker. DaemonFooter → **Connections**
      opens a picker modal (`ConnectionPicker.tsx`): Local / saved remote
      profiles / "Add remote (paste code)". Boot auto-connects to the last
      target (default Local — zero new friction on the desktop). `connect()`
      branches local→`ensureDaemonStarted` vs remote→`connectRemote`; the target
      is persisted (`rt:connection-target`) and self-heals to local if its
      profile is missing. Unreachable remote → stay + retry (the footer error
      state keeps the picker reachable). "Add remote" auto-saves a host-named
      profile then connects; re-pairing the same host:port updates in place.
      Remote mode relabels footer "Restart"→"Reconnect" (never WS-shutdowns the
      remote daemon) and hides the local-only "Stop daemon".
- [x] Phase 6 — remote-mode UI degradation. A `RemoteModeContext` (provided at
      the main App render) + a `rt:remote-unavailable` window-event toast let
      deeply-nested components gate local-FS actions without prop-drilling.
      Gated: add-repo (Sidebar button/link/empty-state, SpawnDialog, add-repo-path
      handler), create-workspace (Sidebar), reveal-in-explorer (App handler,
      SessionContextMenu, WorktreeCleanupFailedDialog), open-in-VS-Code (Sidebar
      container menu, terminal file links — URLs stay enabled), VS Code workspace
      suggestion (suppressed), and every directory/file Browse/Pick (Standalone
      shell, PresetLaunchDialog ×4 — those readonly fields become typeable so a
      host path can still be entered). **Known follow-up:** pop-out windows render
      their own App instance outside the provider, so a terminal file link in a
      remote pop-out isn't gated yet (no regression — it was never gated).
- [x] Phase 7a — per-client layout plumbing. `PersistedState.tabs` →
      `layouts: HashMap<client_id, ClientLayout>` (+ `legacy_tabs` migrated via
      serde alias). Hello carries `client_id`/`client_name` (new `get_client_identity`
      Tauri command persists a per-install UUID + hostname); every tab handler,
      the scoped `tab_events` forwarder, `push_initial_state`, preset launch, and
      the startup prune are per-client. First client after upgrade adopts the
      legacy layout (desktop keeps its tabs); later clients start empty.
- [x] Phase 7b — explicit first-connect chooser. New `client_id` → daemon sends
      `LayoutInitRequired` (has_legacy / active_session_count / clonable) instead
      of `Tabs`; the `LayoutChooser` modal offers start-empty / open-all-sessions /
      adopt-legacy / clone-another-client; the `InitLayout` reply seeds the layout
      (`tabs::clone_tabs_fresh_ids` for clones — same sessions, fresh pane ids) and
      the daemon answers with `Tabs`. Legacy/no-id clients skip the chooser.
- [x] Phase 7c — session-removal auto-close. `tabs::close_session_panes` closes
      panes bound to the removed session (sibling-fill, tab removed if last) across
      every layout, emitting scoped `TabUpdated`/`TabRemoved`. Replaces the old
      prune-to-placeholder.
- [ ] Phases 8–9 — see below.

## Context

The user wants to sit at a laptop on the same LAN and natively control shells that
actually run on the desktop's hardware, against the desktop's source repos — like a
"VS Code Remote" bridge, **not** RDP (RDP forces dealing with monitor-size/DPI
mismatch; we want xterm to reflow natively to the laptop). Concretely:

- Connect from the laptop and **seamlessly pick up shells already running** on the
  desktop (e.g. 4 open shells) without terminating them — see them, send input,
  spawn more, kill them.
- The desktop daemon must be reachable **any time**, including after a reboot —
  so it must auto-start on login (host-side).

### Key finding from exploration: most of this already exists

The daemon is **already fully multi-client**, and "attach to an already-running
session" is already implemented:

- Sessions are **global to the daemon** — no per-client ownership, no ACL. Any
  authenticated client can list, attach, send input, resize, and kill any session
  (`crates/daemon/src/server.rs` SendInput/LoadScrollback handlers; SessionSnapshot
  has no owner field).
- On connect, `push_initial_state` (server.rs:557) sends the full `Sessions` list
  (server.rs:858). **Sessions are decoupled from layout**: a `GridNode::Pane` holds
  `session_id: Option<String>`, a session can exist with **no pane anywhere**, and
  the sidebar lists all sessions regardless of layout — so "attach only to the ones
  I care about" and "later attach to sessions the host never paneled" are already
  expressible concepts.
- Tab/pane layout is **today** global daemon state (`PersistedState::tabs` in
  `state.json`), mutated via tab messages and broadcast to every client via the
  `tab_events` channel (server.rs:577 `spawn_tab_forwarder`). The grid logic lives
  in `crates/daemon/src/tabs.rs` (split/close/move/merge/rearrange). **This plan
  changes layout to per-client** (see Phase 7) while keeping that Rust logic and the
  global session registry intact.
- `LoadScrollback` (server.rs:1526 → `load_scrollback_and_attach_forwarder`
  server.rs:2502) **is** the attach operation: it replays the on-disk scrollback
  ring (`scrollback.rs`, 2 MB) and spawns a per-client live forwarder off the
  session's `pty.output` broadcast. Two clients each get an independent receiver —
  desktop + laptop watch the same shell live.
- The daemon is **long-lived and detached**: there is no window-close/exit handler
  that stops it (`apps/tauri-app/src-tauri` has no `on_window_event`/`RunEvent`
  daemon-kill), so it survives the desktop app closing.

So this feature is mostly **unblocking the transport + bootstrapping a remote
client**, not building session machinery.

### The four real gaps

1. Daemon binds **`127.0.0.1:0` only** (server.rs:365) — unreachable over LAN.
2. The client only knows how to spawn/read a **local** daemon (`ensure_daemon_started`
   → reads local `daemon.json`; `connectDaemon` hardcodes `ws://127.0.0.1`,
   api.ts:148). A laptop can't read the desktop's `daemon.json`.
3. Transport is **plaintext `ws://`** and the token = remote code execution. We chose
   **TLS/WSS + self-signed cert pinning (TOFU)**.
4. Many client actions assume **client and daemon share a filesystem** (file picker
   add-repo, reveal-in-explorer, open-in-vscode) — these target the laptop's FS, not
   the desktop's.

### Settled design decisions (from interview)

- **Client form**: reuse the same Tauri app on the laptop; skip `ensure_daemon_started`,
  connect to the remote daemon, get the Sessions list, attach.
- **Discovery**: manual connection-code first (Phase 1 baseline), mDNS + pairing code
  later (Phase 2 convenience).
- **Security**: TLS/WSS with self-signed cert + fingerprint pinning (TOFU). LAN bind
  is **opt-in, off by default** (loopback stays the default = secure).
- **Scope**: shell remoting only for MVP. Local-FS actions are hidden/disabled in
  remote mode.
- **Per-client layouts** (added after the global-mirror approach was rejected):
  sessions stay global; each client has its **own** tab/pane layout. Storage is
  **daemon-side, keyed by a stable `client_id`** (reuses `tabs.rs` wholesale, layouts
  persist on the host, the desktop is just one client). A brand-new client's first
  connect shows a **chooser** (start empty / clone another client's layout / open all
  active sessions); later connects restore that client's saved layout. When a session
  is killed by anyone, panes referencing it **auto-close** in every client's layout
  (sibling fills, tab closes if it was the last pane).
- **Auto-start**: start the daemon **on login** in the user's interactive session
  (HKCU `Run` key or Task Scheduler "at logon") — full `~/.claude` + PATH + profile
  compatibility. (A SYSTEM service was rejected: SYSTEM/session-0 can't see the
  user's claude creds.)

## Critical architecture decision: TLS must terminate in the Tauri Rust process

The frontend opens the WS via the **webview's JS `WebSocket`** (api.ts:149). On
WebView2 there is **no JS API to pin or trust a self-signed cert** — a `wss://` to
the self-signed daemon would simply fail the TLS handshake. Therefore:

**The pinned-TLS connection terminates on the Rust side of the Tauri app, bridged to
the frontend over loopback.** A tiny TLS-terminating TCP tunnel inside the Tauri
process:

```
frontend  ws://127.0.0.1:<bridgePort>/ws   (plaintext loopback, unchanged JS path)
   │
   ▼  Tauri Rust: connect_remote()  — copy_bidirectional
   │
   ▼  wss://<desktop-ip>:<lanPort>/ws  (rustls ClientConfig, custom verifier pins fp)
remote daemon
```

This keeps the frontend's connection code essentially unchanged: `connectDaemon`
still dials `ws://127.0.0.1:<port>` — only now `<port>` is the local bridge, and the
handshake's `auth_token` comes from the saved remote profile instead of local
`daemon.json`. The tunnel is a byte-level TLS-terminating proxy (`copy_bidirectional`
between a loopback `TcpStream` and a `tokio_rustls` TLS stream) — protocol-agnostic,
no WS frame parsing. Hostname verification is irrelevant because we pin the exact
leaf-cert fingerprint; pass a dummy SNI.

## Implementation phases

> Track this as `docs/plans/remote-lan-access.md` once work starts (project
> convention; tick checkboxes there). Protocol additions follow the
> `add-protocol-message` skill (4-file pattern). All additions here are **additive**
> (new message types absorbed by the `Unknown` wrappers; new struct fields with
> `#[serde(default)]`) → **no `protocol-version.json` bump**.

### Phase 0 — Persist the auth token when LAN is enabled  *(prerequisite)*
- [ ] Today the token is regenerated every daemon start (server.rs:299). A laptop's
      saved profile would break on every desktop reboot. When LAN mode is enabled,
      **load-or-generate a persistent token** stored in the new LAN config, and have
      `write_handshake` use it. Local loopback clients read whatever the current
      token is, so they're unaffected.
- Files: `crates/daemon/src/server.rs` (token init), new `crates/daemon/src/lan.rs`.

### Phase 1 — Daemon: persistent LAN config + self-signed cert + TLS listener
- [ ] New `crates/daemon/src/lan.rs`: read/write `lan.json` (`{ enabled, port }`) +
      the persisted token; generate & persist `lan-cert.pem` / `lan-key.pem` via
      `rcgen` on first enable; compute SHA-256 fingerprint of the leaf cert.
- [ ] Add the cert/key/config paths to `crates/daemon/src/paths.rs` (alongside
      `handshake_file`).
- [ ] In `server::run`, when LAN is enabled, **start a second listener** bound to
      `0.0.0.0:<port>` with `tokio-rustls`, serving the **same** `Router`/`Hub`
      (refactor the `Router::new()...with_state(hub)` at server.rs:358 into a builder
      reused by both listeners; drive both with `tokio::select!` + the existing
      graceful-shutdown signal). Loopback `ws://` listener (server.rs:365) is
      **unchanged**. The token handshake (handshake() server.rs:777) works over TLS
      verbatim.
- [ ] Detect non-loopback IPv4 addresses to report to the UI (std interface
      enumeration or `local-ip-address` crate).

### Phase 2 — Protocol: LAN config, status, + client identity
- [ ] `ClientMessage::ConfigureLan { enabled, port }` and
      `DaemonMessage::LanStatus { enabled, port, fingerprint, addresses }` in
      `crates/protocol/src/lib.rs`.
- [ ] Add `client_id: Option<String>` (+ optional `client_name`, e.g. hostname) to
      the `Hello` message (server.rs:1301). `#[serde(default)]` → additive. The
      daemon records the connection's `client_id` for layout scoping (Phase 7). A
      missing/`None` id falls back to the legacy global layout so old clients still
      work. The `client_id` is generated once per install and stored durably client
      side (new Tauri command `get_client_id` backed by a config-dir file; shared by
      a window and its pop-outs via the same install).
- [ ] Daemon handler in `server.rs`: persist config, bring the TLS listener up/down,
      emit `LanStatus` (broadcast, so all clients see the state).
- [ ] TS mirror in `apps/tauri-app/src/types.ts` + dispatcher in `api.ts`/`App.tsx`.

### Phase 3 — Desktop UI: enable LAN + copyable connection code
- [x] Settings panel/modal: "Enable LAN access" toggle, port field, live status
      (detected addresses, cert fingerprint). On enable, assemble a single
      **connection code** = base64url(JSON `{ v, host, port, token, fp }`) — the
      desktop already holds its token (local `daemon.json`); combine with `LanStatus`.
      One copyable string instead of four hand-copied fields.
- [ ] Optional "Regenerate token" button (revokes laptop access; forces re-pair).

### Phase 4 — Tauri Rust: pinned-TLS loopback tunnel + remote-profile store
- [x] `connect_remote(params) -> DaemonHandshake` command: start the loopback
      TLS-terminating tunnel (listen `127.0.0.1:0`; per-accept open a `tokio_rustls`
      TLS conn to `host:port` with a custom `ServerCertVerifier` that accepts
      **only** the pinned fingerprint; `copy_bidirectional`) → return a synthetic
      `DaemonHandshake { port: bridgePort, auth_token: <profile token>,
      protocol_version, pid: 0 }`. An eager TLS probe (8 s timeout) validates
      reachability + pinning before the bridge port is returned. `connect_remote`
      takes resolved `ConnectionParams`; `decode_connection_code(code)` is the pure
      base64url decode; `disconnect_remote` aborts the active bridge.
- [x] Remote-profile persistence: `remote-profiles.json` in the config dir +
      commands `list_remote_profiles` / `save_remote_profile` / `delete_remote_profile`.
- Files: `apps/tauri-app/src-tauri/src/lib.rs` (commands + handler registration),
  new `apps/tauri-app/src-tauri/src/remote.rs`.

### Phase 5 — Frontend: connection-mode launcher + reuse the connect path
- [x] Connection picker reachable from the DaemonFooter (**Connections**), not a
      pre-main gate — keeps the desktop's zero-friction boot. Modal
      (`ConnectionPicker.tsx`): **Local (this machine)** / saved remote profiles
      / **Add remote (paste code)**. Last choice persisted (`rt:connection-target`)
      for auto-connect.
- [x] In the `connect()` effect: `resolveHandshake()` branches — local →
      `ensureDaemonStarted()`; remote → `connectRemote(params)`. Both yield a
      `DaemonHandshake`; downstream `connectDaemon(handshake)` and the
      Hello/Sessions/attach flow are **unchanged** (`ws://127.0.0.1:${port}`
      already works for the bridge). A missing remote profile self-heals to local.
- [x] `isRemote` derived from `connectionTarget`, threaded to the footer (relabel
      Restart→Reconnect, hide local-only Stop). Full local-FS gating is Phase 6.

### Phase 6 — Remote-mode UI degradation (hide local-FS actions)
- [x] Gate on `isRemote` via `RemoteModeContext` + `notifyRemoteUnavailable`
      (window-event toast). Covered: add-repo, add-repo-path, create-workspace,
      `reveal_in_explorer`, `open_path_in_vscode`, `open_folders_in_vscode`, the VS
      Code workspace suggestion, and all directory/file Browse/Pick affordances.
      Buttons disable + tooltip; readonly path fields become typeable so a host
      path can still be entered; click-only handlers (terminal links) toast.
      Touch points: `App.tsx`, `Sidebar.tsx`, `Terminal.tsx`,
      `SessionContextMenu.tsx`, `SpawnDialog.tsx`, `StandaloneShellDialog.tsx`,
      `PresetLaunchDialog.tsx`, `WorktreeCleanupFailedDialog.tsx`, new
      `utils/remoteMode.ts`.

### Phase 7 — Per-client layouts (the core of "control only what I care about")

Decouple **global sessions** from **per-client layout**. Sessions stay shared; each
`client_id` gets its own tab/pane layout, stored daemon-side, reusing all of
`tabs.rs`. This is the largest phase and can be built/tested locally (two installs,
or a `client_id` override) independent of the TLS transport.

**Daemon — state model (`crates/daemon/src/state.rs`)**
- [ ] Replace `PersistedState::tabs: Vec<TabEntry>` with
      `layouts: HashMap<String, Vec<TabEntry>>` (key = `client_id`). Keep the old
      `tabs` field as a read-only `legacy_tabs` migration source (`#[serde(default)]`)
      — see migration below. Sidebar ordering (`container_order`, `session_order`)
      stays **global** (it orders global sessions).

**Daemon — per-client routing (`crates/daemon/src/server.rs`)**
- [ ] Every tab-mutation handler (CreateTab/CloseTab/SplitPane/ClosePane/MovePane/
      MergeTabs/RearrangeTab/SetPaneRatio/ReplacePaneSession/ExtractToNewTab/
      ReorderTabs, ~server.rs:1609-1806) operates on `layouts[client_id]` for the
      **sending connection's** `client_id` instead of the global `s.tabs`. The
      `tabs.rs` functions are reused unchanged — only the slice they mutate changes.
- [ ] Scope `tab_events`: tag each event with its owning `client_id`
      (`ScopedTabEvent { client_id, event }`) and filter in `spawn_tab_forwarder`
      (server.rs:577) so a connection only receives its own layout's events. A window
      and its same-machine pop-outs share a `client_id` → they stay in sync as today.
- [ ] `push_initial_state` (server.rs:835) sends `DaemonMessage::Tabs` for **this
      client's** layout (`layouts[client_id]`), not the global list.

**Daemon — session-removal fan-out (auto-remove, per the interview)**
- [ ] On `SessionRemoved`, iterate **all** `layouts`; in each, close panes that
      reference the removed `session_id` via `tabs::close_pane` (sibling fills, tab
      removed if it was the last pane); emit a scoped `TabUpdated`/`TabRemoved` per
      affected `client_id`. This replaces today's "null the session_id to an empty
      placeholder" on removal. (Intentionally-created empty panes are unaffected.)

**First-connect chooser (`client_id` not yet in `layouts`)**
- [ ] New messages: `DaemonMessage::LayoutInitRequired { has_legacy: bool,
      active_session_count: u32, clonable: Vec<{ client_id, name }> }` and
      `ClientMessage::InitLayout { kind: Empty | CloneLegacy | CloneClient(id) |
      AllSessions }`. On an unknown `client_id` the daemon sends `LayoutInitRequired`
      instead of `Tabs`; the frontend shows a modal chooser; the reply creates
      `layouts[client_id]` (clones reuse `tabs.rs` to deep-copy a layout with **fresh
      `pane_id`s, same `session_id`s**; `AllSessions` builds one pane per running
      session) and the daemon then sends `Tabs`. An entry that exists but is an empty
      `Vec` is a valid saved state (no chooser).

**Migration**
- [ ] On upgrade, existing global `tabs` land in `legacy_tabs`. The first client whose
      `client_id` is unknown sees `has_legacy: true`; choosing `CloneLegacy` adopts
      the previous layout into its `client_id` and clears `legacy_tabs` (offered once).
      The desktop user keeps their tabs; later clients get the normal chooser.

**Frontend**
- [ ] Send `client_id` in Hello (from `get_client_id`). Handle `LayoutInitRequired`
      with a chooser modal; send `InitLayout`. Otherwise the existing `tabs` /
      `tab_updated` / `tab_removed` / `tabs_reordered` reconciliation in
      `App.tsx::handleMessage` is **unchanged** (it already renders whatever layout
      the daemon sends). `activeTabId`/focus are already client-local (localStorage
      `rt:active-tab:main`) — naturally per-client.

**Notes**
- The single-desktop experience is **unchanged**: one client = one layout = today's
  behavior, with its tabs migrated in.
- "Pick up the same shells" is now a *choice* (chooser → clone host / open all), not
  a forced mirror — exactly what the user asked for.
- PTY resize across two simultaneously-attached clients is still last-writer-wins
  (no arbitration; cf. memory `no-raf-timer-coalescing`). With per-client layouts the
  laptop typically views a different/smaller set, and when it's the only viewer of a
  shell its size governs cleanly — the RDP-DPI problem the user wanted to avoid.

### Phase 8 — Host auto-start on login
- [ ] Tauri commands `set_autostart(enabled)` / `get_autostart()` registering the
      **daemon binary** (resolved like `daemon_supervisor` resolves it) in HKCU
      `Software\Microsoft\Windows\CurrentVersion\Run` (or a Task Scheduler "at logon"
      task). Runs in the user's interactive session → full claude-CLI compatibility.
      Desktop settings toggle. (Launch the daemon directly, not the app — the daemon
      reads its persisted `lan.json` and brings the LAN listener up on its own.)

### Phase 9 — Discovery Phase 2: mDNS + pairing code  *(convenience, after MVP)*
- [ ] Daemon advertises `_rustling-tulip._tcp` (e.g. `mdns-sd`) with TXT records
      (port). Laptop browses + lists hosts.
- [ ] Pairing: desktop shows a short code; laptop enters it; a short-lived pairing
      exchange over TLS transfers token + fingerprint so the long token is never
      hand-copied. Becomes the default convenient path; manual code remains the
      fallback.

## Dependencies & guardrails (validate FIRST)

New crates: `rustls`, `tokio-rustls`, `rustls-pemfile` (daemon + Tauri side), `rcgen`
(daemon), later `mdns-sd` and optionally `local-ip-address`.

- **`cargo deny check` risk** — the chosen TLS crypto backend matters.
  `rustls 0.23` defaults to `aws-lc-rs`; `rcgen` can pull `ring`. `ring`'s license
  is a non-SPDX mix that `cargo-deny` may flag against the current `deny.toml` allow
  list (MIT/Apache/BSD/ISC/MPL/Unicode/Zlib/CC0). **Decide the backend and run
  `cargo deny check` before writing real code** (per the "verify guardrails first"
  principle); add a documented license exception only if unavoidable.
- Keep `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt`,
  `pnpm typecheck` green throughout (workspace denies `unwrap_used`/`panic`/etc — the
  tunnel and cert code must use `?`/`anyhow::Context`, no `.unwrap()`).

## Verification

- **Unit**: connection-code encode/decode round-trip; cert fingerprint stability;
  pinned `ServerCertVerifier` accepts the matching cert and rejects a different one.
- **Integration (single machine)**: start the daemon with LAN enabled on
  `127.0.0.1:<lanPort>` (TLS); run `connect_remote` against it; confirm the tunnel +
  Hello + `Sessions` round-trip. Rust integration test under `crates/daemon` (or a
  tauri-side test for the tunnel).
- **Per-client layout**: with two distinct `client_id`s against one daemon (two
  installs, or a dev `client_id` override), confirm: each gets its own first-connect
  chooser; layouts diverge (a split on A doesn't appear on B); both see all sessions
  in the sidebar; killing a session on A auto-closes its panes on **both**;
  `CloneLegacy` preserves the pre-upgrade desktop tabs once; a window + its pop-out
  share one layout. Unit-test the migration + clone (fresh pane_ids, same session_ids)
  in `crates/daemon`.
- **Manual two-machine acceptance** (the real test):
  1. Desktop: open ≥4 shells, enable LAN access, copy the connection code.
  2. Laptop: launch the app, choose "Add remote", paste the code → first-connect
     chooser. Pick **start empty**, then add only the 2 shells you care about into
     your own tabs. Reconnect → your curated layout returns (not the desktop's).
  3. On the laptop add a session the desktop never paneled (it's in the sidebar) →
     it opens in your layout only. Spawn a new one, kill one; confirm input/output
     reach the desktop PTYs and the killed session's panes auto-close on both ends.
  4. Confirm the cert is pinned (tamper the fingerprint → connection refused).
  5. Reboot the desktop, log in (don't open the app) → daemon autostarts → laptop
     reconnects and restores its own layout over the still-running shells.
- **Lints/CI**: `cargo clippy -D warnings`, `cargo fmt --check`, `cargo deny check`,
  `pnpm typecheck`.

## Out of scope (MVP)

- Remote repo management / daemon-side directory browser (chose shell-remoting only).
- Multiple named layouts per client / switchable layout profiles (one layout per
  `client_id` for now — the per-client model leaves room for this later).
- Multi-machine attach beyond LAN, cloud sync, internet exposure (LAN + TLS pinning
  only).
- SYSTEM Windows service (rejected — breaks per-user claude creds).
