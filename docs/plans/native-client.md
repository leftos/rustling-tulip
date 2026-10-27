# Native client: open items' rulings

The rulings the native client's open items need before they are briefed. Each item is a Linear issue, ordered by its wave's project (snapshot: [MAIN.md](./MAIN.md)); the client's settled design is in [docs/native-client.md](../native-client.md), and what each feature must do is in [native-client-parity.md](./native-client-parity.md), whose lines are ticked as items land. An item's section here is deleted in the commit that lands it; the file goes when the last one does.

## Working rules

- **Landing:** each item is committed to `main` as soon as it is green, and pushed at checkpoints (a finished wave), not after every item.
- **Autonomy:** drive the native client to done without stopping between items or waves. Technical decisions are settled in the session and recorded here as rulings; only questions about behaviour the user will see go to the user. Phase 6 is split into brief-sized items from its parity section when its wave comes up, and the split goes to the user before anything is dispatched.
- **Phase 4 scope:** its named features plus every parity line not claimed by Phase 6 (remote). Every item mounts into `RootView` (`lib.rs`), so concurrent items rebase onto `main` before landing. Concurrency ceiling 3. P4.12c builds on the spawn form's target lock (`spawn_form.rs` `Lock`: a locked target or tab, an optional pinned worktree and `share_confirmed`), which P4.4b added for "Launch session here".

## Phase 6 — remote and cutover

- Remote mode follows steps P6.1 to P6.7 of [remote-access.md](./remote-access.md). Landed: the transport seam (P6.1), remote-mode rules and typed host paths (P6.2a, P6.2b), the host picker (P6.3) and the Remote access panel (P6.4).
- Autostart (P6.5) has landed as `daemon_client::autostart`. Remote tests, the hand checklist and the docs pass (P6.6) have landed. The installer (P6.7) is open.
- Cutover: the installer ships the native client, and on uninstall deletes the daemon's `rustling-tulip-daemon` login `Run` value; update CLAUDE.md.
