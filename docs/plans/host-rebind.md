# Unbind and rebind a host's sessions (RT-1594)

Owner's request, verbatim: "I'd like to be able to quickly unbind all currently bound sessions from a given machine, and bind them again, given the option to auto-split across tabs and auto-layout (grid, side by side, stacked, up to how many panes per tab), or to follow that machine's tab and pane layout. This should work for both the daemon local to the client, as well as remote machine daemons."

Every decision below is an orchestrator decision (not an owner ruling), settled from the code.

## Terms

- **Host**: This PC or a remote daemon the client is connected to; on the wire `host: Option<String>`, `None` being This PC, as every pane's `host` field is.
- **Layout home**: This PC's daemon, which keeps every tab of this client (`ClientLayout` in `state.json`), remote sessions included as panes with `host` set.
- **Active session**: a session of the host whose status is not `Stopped` or `Error`, that is not inactive (parked) or abandoned, and whose role is a normal session (not an account login). An orphan is active.

## What exists

- Unbinding one pane is `ClosePane` with "Close pane but keep session in sidebar"; the session keeps running and shows in the Tabs view's Unbound container. A tab whose last pane closes is removed. `ForgetHostSessions { all: true }` closes every pane of a remote host, follow panes included, and cannot name This PC.
- The first-connect chooser (`apps/native/src/layout_chooser.rs`, `layout_chooser_view.rs`) picks a layout (Grid with a shape or Auto, Side by side, Stacked) and a maximum per tab, then splits one tab with `ExtractToNewTab` and `RearrangeTab` after the daemon answers, in two phases because pane ids exist only then.
- No message returns another client's layout. A daemon's `ClientLayout` has a `name`, the hostname of the client that made it, and no kind or "local" flag.

## Decisions

### Wire (additive, no bump)

- `ClientMessage::UnbindHostPanes { host: Option<String>, request_id: Option<String> }`, sent to This PC: closes every pane of this client's tabs whose `session_id` is a session of `host`. Follow panes (a dispatch run, a subagent) stay: they are not sessions. Emptied tabs are removed and untouched tabs rebuilt, as `ForgetHostSessions` does; answered by `TabUpdated` / `TabRemoved`, an `Error` with the `request_id` on failure.
- `ClientMessage::BindHostSessions { host: Option<String>, session_ids: Vec<String>, arrangement: HostArrangement, request_id: Option<String> }`, sent to This PC: builds new tabs at the end of the tab list holding those sessions, in one step, and answers with a `TabUpdated` per new tab. Sessions already shown by a pane of this client are skipped.
- `HostArrangement` (tagged, `#[serde(other)] Unknown`): `Auto { layout: RearrangeLayout, max_per_tab: Option<u32> }` or `Follow { tabs: Vec<TabEntry> }`, the host's own tabs as its daemon sent them.
- `ClientMessage::ListHostLayouts { request_id }` answered by the host's own daemon with `DaemonMessage::HostLayouts { host_name: String, layouts: Vec<HostLayout>, request_id }`, `HostLayout { client_id, name, tabs: Vec<TabEntry> }`: every other client's non-empty layout on that daemon, and the daemon's own hostname. Listed in `Welcome.features` as `host_layouts`; a client asks only a host that lists it. Tailnet clients may ask (read-only ids and names, the same trust as the session list).
- "The host's own layout" is the layout whose `name` equals `host_name`: a client names its layout after its machine's hostname, so the window running on the host made it. The dialog preselects it and lists the others. No `state.json` change.

### Daemon (This PC, the layout home)

- Pure functions in `crates/daemon/src/tabs.rs`, each unit-tested: `unbind_host_panes(layout, host)`, `auto_host_tabs(session_ids, layout, max_per_tab)` and `follow_host_tabs(host_tabs, host, session_ids)`.
- **Auto**: sessions in the order the client sends them, chunked by `max_per_tab` (`None` = one tab), each tab built with the existing layout builders and `picked_layout` set to the chosen layout, named by the daemon's default tab naming.
- **Follow**: each of the host's grid tabs is copied with fresh tab and pane ids, its split tree and ratios, its name and `picked_layout`; each pane gets `host` stamped; a pane is kept only when its session is in `session_ids` (a pane with no session, a follow pane, or a pane naming a third host is dropped, and its sibling fills the space); a non-grid tab and a tab left empty are skipped. Sessions in `session_ids` the host's layout does not place go into Auto Grid tabs of at most four panes after the copied ones, so every active session is bound.

### Client

- The action takes **two steps in one dialog**: "Unbind {host}'s sessions" (sends `UnbindHostPanes`) and "Rebind {host}'s sessions…" (opens the dialog, whose confirm sends `UnbindHostPanes` then `BindHostSessions` with the host's active sessions in their sidebar order).
- The dialog reuses the chooser's picker (Grid with shapes and Auto, Side by side, Stacked; Max per tab), extracted into a shared module, plus a "Follow {host}'s layout" choice listing the host's layouts (the host's own first), shown only when the host lists `host_layouts` and has a non-empty layout. Nothing is remembered between openings.
- Entry points: the host's row in the Connections view (which lists This PC too) gets **Unbind** and **Rebind…** controls; the jump palette gets one "Unbind {host}'s sessions" and one "Rebind {host}'s sessions…" row per host; the sidebar's host header (shown when more than one host is configured) gets a right-click menu with the same two items. An offline host's controls are disabled with "{name} is offline".
- Pop-outs are not tab panes and are left as they are. The new tabs take no focus; the active tab stays unless it was removed, when the first new tab shows.

## Steps

- [ ] **HR.1 Protocol.** The three client messages, `HostArrangement`, `HostLayouts`/`HostLayout`, `FEATURE_HOST_LAYOUTS`; round trips and old-decode tests; `apps/pwa/src/protocol.ts` regenerated. Files: `crates/protocol/src/lib.rs` (hotspot), `apps/pwa/src/protocol.ts`.
- [ ] **HR.2 Daemon.** The three pure functions and their tests; the `UnbindHostPanes` and `BindHostSessions` handlers in the tabs domain module; the `ListHostLayouts` handler and the feature in `Welcome`. Files: `crates/daemon/src/tabs.rs`, `crates/daemon/src/server/tabs.rs`, `crates/daemon/src/server.rs` (hotspot: the dispatch or-pattern), `crates/daemon/src/server/handshake.rs`, `crates/daemon/src/state.rs` (read only). Depends on HR.1.
- [ ] **HR.3 Client model.** The picker extracted from `layout_chooser.rs` into a shared module; `apps/native/src/host_rebind.rs`: the active-session filter, the order, the dialog model and the messages, unit-tested. Files: `apps/native/src/layout_chooser.rs`, a new picker module, `apps/native/src/host_rebind.rs`, `apps/native/src/lib.rs` (hotspot: the `mod` line). Depends on HR.1.
- [ ] **HR.4 Client dialog and entry points.** `apps/native/src/host_rebind_view.rs`, the Connections row controls, the jump palette rows, the host header menu, `HostLayouts` and `Error` handling in `on_message`; UI specs in a new `apps/native/tests/ui/ui_host_rebind.rs`. Files: those, `apps/native/src/connection_picker_view.rs`, `apps/native/src/quick_jump.rs`, `apps/native/src/sidebar_view.rs`, `apps/native/src/lib.rs` (hotspot). Depends on HR.2 and HR.3.
- [ ] **HR.5 Docs and shot.** `docs/native-client.md` ("Tab layouts", the host header, Connections, Jump palette), `docs/wire-protocol.md`, a `host-rebind` `native-shot` view, the glossary's "Unbound"; then this file's decisions are promoted and it is deleted. Depends on HR.4.
