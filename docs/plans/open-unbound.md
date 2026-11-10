# Open a machine's unbound sessions at once

Today a session no pane shows is "unbound": its sidebar row carries an `unbound` pill, and a click opens that one session in a new tab. Laying out many of them means opening each and arranging the tabs by hand. This feature opens every unbound session of one machine in one step, laid out by the existing layout chooser.

## Owner rulings

- **Layout:** the action opens the existing layout chooser (the one a new client's first connect shows), pre-filled with the session count: the user picks a shape and a max panes per tab, and sessions past the max go to extra tabs. No new look.
- **Which sessions:** every unbound session with a terminal that is still running, agent sessions and plain shells alike. Stopped and errored sessions, and headless ones, stay unbound.
- **Where:** a menu on the sidebar's Unbound group header (Tabs view), an item on each machine's header in the Repos view, and an "Open unbound on {host}" row per machine in the jump palette.
- **Placement:** release vNext+1.

## Orchestrator decisions (open to the owner)

- **The daemon builds the tabs**, in one change to the requesting client's layout, through a new additive message: `ClientMessage::OpenSessionsInTabs { sessions: Vec<{ host, session_id }>, layout: RearrangeLayout, max_per_tab, aspect, request_id }`, offered when `Welcome.features` lists `open_sessions_in_tabs`, answered `DaemonMessage::SessionsOpened { request_id, tab_ids }` or an `Error` echoing `request_id`. It splits the list into tabs of at most `max_per_tab` in order and builds each with the same functions the layout chooser's arrangement and `RearrangeTab` use (`build_grid` and its neighbours in `crates/daemon/src/tabs.rs`), recording the picked layout so later splits re-grid as for any picked tab. Layout always lives on This PC; a remote host's session is a pane with its `host` set, as today.
- **The list is read at click time** from the client's pill map (`tab_pills`), grouped in the sidebar's order (repo, then workspace, then the rest), so the grid follows the sidebar. A session bound or ended before the daemon applies the change is skipped by the daemon (a session already shown in a pane, or not running).
- **Which machine:** the Unbound group's menu acts on This PC (the group lists only This PC's sessions); a host header and a palette row act on their own machine. The action is offered only while that machine has an unbound running session, and its label carries the count: `Open {n} unbound sessions…`.
- **Tab names:** `Unbound` for This PC and `Unbound on {host label}` for a remote machine, with ` 2`, ` 3` for overflow tabs, as preset launch names them.
- **No undo entry:** closing the new tabs puts the sessions back to unbound.

## Steps

- [ ] The daemon message: protocol types, `DAEMON_FEATURES`, the handler in `crates/daemon/src/server/tabs.rs` over a pure function in `crates/daemon/src/tabs.rs`, the generated `apps/pwa/src/protocol.ts`, `docs/wire-protocol.md`.
- [ ] The client action: the chooser reused for this action (`apps/native/src/layout_chooser.rs`), the three entry points (`session_menu.rs` `MenuContainer` for the Unbound group, the host header in `sidebar_view.rs`, `quick_jump.rs` rows), the reply's toast on refusal, specs in `ui_sidebar.rs`, `ui_layout_chooser.rs` and the jump palette's spec, and a `native-shot` view.

## Related

- RT-462 (Plan auto relayout: reorganize every tab's panes by rules, with a preview) shares the layout rules.
- RT-613 (Close a parked remote session's pane in This PC's layout) covers remote panes left by a disconnected host.
