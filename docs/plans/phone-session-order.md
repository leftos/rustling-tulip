# Phone session order

Design for RT-2034: the phone's Home list can be sorted and organized. Owner ask: "Allow me to sort and organize the sessions in the mobile app. Alphabetically, surface needs you, mirror tab layout of client the daemon is running on as a 2-level list, etc". Everything below beyond that ask is an orchestrator decision, open to the owner.

## Orders

The phone keeps one order for its whole Home list, `UiState.phone_order` in the phone's own `native-ui.json`. Hosts stay the top level in every order, in their current order; the order applies inside each host.

- **Manual** (the default, and what a missing or unknown value loads as): today's list, every row in a fixed place; urgency is a mark, not a position.
- **A–Z**: the host's container groups by name, case-insensitive, and the sessions in each by label, case-insensitive; Detached stays last.
- **Needs you first**: a group "Needs you" opens the host, holding every session the Needs you set holds (`needs_you.rs`), in manual order; the container groups follow in manual order without those sessions. A session moves into and out of the group as its state changes. A host with no such session has no "Needs you" group.
- **Host's tabs**: a two-level list mirroring the tab layout of the desktop client on that host: one group per tab of the host's own layout, headed by the tab's name, in tab order, holding that host's sessions in the tab's panes in pane order (a session shown twice is listed once, at its first pane). A tab with no session of that host is left out; a tab that is not a grid is left out. A final group "Not in a tab" holds the host's other sessions in manual order. A host whose own layout is unknown (an older daemon without `host_layouts`, no layout yet, or none matching) shows its manual order with the note "No desktop layout on this host" under its header.

## The host's own layout

The phone sends `ListHostLayouts` to each open host that lists `host_layouts`, while the order is Host's tabs: on the host's Welcome, on return to the foreground, and when the order is switched to Host's tabs. From the `HostLayouts` reply it keeps the layout whose `name` equals `host_name` (the rule `host_rebind.rs` uses for "the host's own"); when no layout matches and the reply holds exactly one, that one. A pane whose `host` is set names another daemon and is skipped. The phone keeps the last reply per host until the next one, and drops it when the host closes.

## The control

A sort button in Home's header, beside the title (on the iPad, in the list pane's header), opens a menu sheet with the four orders, the current one checked: "Manual", "A–Z", "Needs you first", "Desktop tabs". Choosing one saves it and redraws the list.

## Steps

- [ ] RT-2034: the order, the control, the layout fetch, specs and the `phone-home-sorted` shot
