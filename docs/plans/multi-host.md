# Several hosts in one window

One native window shows sessions from This PC and every saved tailnet and SSH host at once, as herdr-gpui does, instead of one window per host behind the Connections picker's target switch. Tracked as RT-118 and its MH.n sub-issues; the work lives on `feat/multi-host`.

## Rulings

Owner rulings from the design interview:

1. Tabs can mix hosts: one tab may split panes from different hosts.
2. This PC's daemon is the layout home: it holds every tab of the window, and a pane names its session as `{host, session_id}`. Remote daemons supply sessions only; their per-client layouts for this client are ignored. This PC is always connected.
3. Every saved host connects at launch and stays connected; Connections gives each host an on/off switch that is remembered. `last_target`, the boot target and `SwitchTarget` are removed.
4. An offline host keeps its sidebar group and panes, dimmed with a reconnecting or offline chip and the last frame frozen; other hosts are unaffected. Only This PC (the layout home) keeps a window-wide overlay.
5. Host-less entry points reach every host: the spawn dialog's picker lists every host's repos and workspaces grouped by host, the quick shell goes to the focused session's host (else This PC), and a container's menus spawn on its own host.
6. A shell on a remote host is one click: `+ Shell` on each host's sidebar header, and a host field in the Shell dialog. It runs on that host's daemon, tracer-backed, with status and history like any session.
7. Host-side Settings pages (agent settings, launch commands, keep awake, remote access, worktrees root, summarizer) get a host selector at the top, defaulting to This PC; client-only pages ignore it.
8. The same daemon reached twice (a tailnet name and an SSH alias) is detected by a daemon instance id in `Welcome`; the second connection is dropped and its row says `same daemon as <host>`.

Orchestrator decisions:

- Quit stops This PC's daemon as today and only disconnects remote hosts.
- Notifications, Needs You and the Dashboard span every host; their rows carry a host chip when more than one host is configured.
- Recover lists history from every host, grouped by host.
- Pairing is per host: a remote host that is not approved shows `Pair` on its sidebar header and Connections row, never a window overlay.
- The host key is `Target::host_key()` (today's `session_count_key`, renamed): a tailnet host's lower-cased MagicDNS name or `ssh:<host string>`, and `None` on the wire for This PC; one formatter serves `host_session_counts`, `HostKey` and the wire `host`, and a tailnet port change does not orphan that host's panes.
- The look of the host headers and chips is decided from the existing palette and shown as a `native-shot` at landing (the ship-fast working mode; no mock round).

## Shape

### Protocol and daemon (additive, no version bump)

- `GridNode::Pane` gains `host: Option<String>` (`#[serde(default)]`, `None` = this daemon). The tab requests that name a session (`CreateTab`, `SplitPane`, `ReplacePaneSession`, the leaf drop's message) and `OpenDiffTab` carry `host` too; `TabContent::Diff` gains `host`. `MovePane`, `SwapPanes`, `ExtractToNewTab` and `MergeTabs` name panes by id, and each pane keeps its own host.
- The daemon never checks a pane's session against its registry, so foreign panes need no exemption there; a foreign diff tab skips `open_diff_tab`'s repo check. The daemon's pane helpers carry `host` through every rebuild, and a local session removal (`close_session_panes`), the startup prune and the rebind touch only `host: None` panes.
- New `ClientMessage::ForgetHostSessions { host, session_ids, all }`: applies to foreign panes what a local session removal does; `all` drops every pane of that host.
- `Welcome.daemon_id`: a UUID persisted in the config dir, created on first start; feature string `daemon_id`.

### Client transport

- `HostKey` (`Local` | `Remote(String)`) in `apps/native/src/target.rs`, built from a `Target`.
- One `Net` thread per connected host, unchanged inside; a per-host forwarder tags each `NetEvent` with its `HostKey`.
- `RootView` holds `hosts: HashMap<HostKey, HostConn>`: `tx`, `Connection`, handshake, and that host's mirrors (agents, agent settings and commands, keep-awake, remote status, worktrees root, history, devices).
- `RootDeps` takes a transport factory, `open(target) -> (tx, events)`, in place of one `tx`/`events` pair: the real one spawns a `Net`, the test one registers fake channels.
- `send(msg)` becomes `send_to(host, msg)` with no default: tab and layout messages go to `Local`, everything else to the owning host, found by the sidebar lookups that return the host.
- `reset_for_target` / `reset_for_new_daemon` become `reset_host(host)`, which drops a host's mirrors (agents, daemon settings, remote status, scans) and what showed its sessions; `clear_host_slice` is the deeper reset when the Local slot's target changes, and a remote host's disconnect clears nothing.

### Per-host state

- `SidebarModel` holds a `HostSlice` per host (repos, workspaces, sessions, agents, and the container and session orders, attention and unseen sets kept from them); a remote host's container keys are `{host key}::{key}`; the `Sessions`, `Repos` and `Workspaces` folds replace only the sending host's slice; id lookups return `(HostKey, &item)`.
- Keyed by `(host, id)`: `seen_sessions` and notifications, dashboard facts (`WatchDashboard` per host), the source-control model and pins, the file-fetch registry (ended per host on its disconnect), `Capabilities` (`remote_mode.rs`), `PopoutKey::Session`.
- Foreign-pane pruning: when a remote host's `Sessions` omits an id a pane references, the client sends `ForgetHostSessions` to Local; nothing is pruned while a host is offline.

### UI

- Connections becomes a host manager: This PC and each saved host with an on/off switch (`saved_hosts[].enabled`, default true), a state chip, `Pair`, `Forget`, and `+ Add host`.
- The sidebar shows a header row per host when more than one host is configured: state dot, `+ Shell`, collapse, that host's repos and workspaces nested under it, dimmed while offline.
- Tab pills and pane headers carry a host chip for remote panes; a remote session can be dragged into any tab.
- The footer pill shows This PC's state plus `n/m hosts online`.

## Items

- [x] MH.1 (RT-563) Protocol and daemon: pane, tab and diff `host`, foreign references in the tab code, `ForgetHostSessions`, `Welcome.daemon_id`.
- [x] MH.2 (RT-564) Transport: `HostKey`, a `Net` per host, tagged events, the `RootDeps` factory, `send_to`; `Harness::connect()` stays Local. Behaviour unchanged.
- [x] MH.3 (RT-565) Per-host state: sidebar slices, per-host mirrors, notifications, dashboard, source control, file fetch, capabilities, Recover, session pop-out keys, `reset_host`; the UI-spec harness gains `Harness::host(target)` sub-daemons; spec `ui_multi_host.rs`.
- [x] MH.4 (RT-566) Connect every enabled host: remove `SwitchTarget`, `last_target` and the boot target; Connections as host manager with switches, per-host pairing and daemon-id dedupe; offline hosts dimmed, the overlay for Local only.
- [x] MH.5 (RT-567) Sidebar host groups with `+ Shell`, host chips on pills and pane headers; shot view `multi-host`.
- [ ] MH.6 (RT-568) Mixed tabs: place remote spawns and duplicates into Local's layout, move and drag remote sessions into any tab, prune foreign panes, host in pane and tab pop-out keys, remote sessions in no tab.
- [ ] MH.7 (RT-569) Cross-host spawn and settings: grouped spawn picker, quick-shell host, the Shell dialog's host field, the Settings host selector, Recover grouped by host.
- [ ] MH.8 (RT-570) Live proof and docs: `e2e_multi_host.rs` (a local daemon plus a second isolated one through fake-ssh and the bridge), the hand checklist, `docs/native-client.md`, `docs/architecture.md`, glossary, CLAUDE.md, the changelog; this file promoted and deleted.
