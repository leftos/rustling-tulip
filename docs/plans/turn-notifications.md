# Needs you and Finished: the only two notifications

The native client used to fire the three OS toasts ported from the Tauri app: "Claude is awaiting input", "Claude session stopped" (the process exited) and "Claude session errored", each with its own toggle in Settings → Notifications.

TN.1 to TN.3 replaced them on the desktop: `notify.rs` raises `Needs you` and `Finished`, each with the session's label as its body and its own toggle, under one skip-when setting (`apps/native/src/notify.rs`, `settings_view.rs`). What stays open here is the mobile half (RT-105, RT-143) and the detection RT-74 sharpens.

## Rulings (owner)

- **Two kinds only.** Every Tauri-era toast goes: the stopped and errored toasts are removed, not renamed, and their toggles leave Settings → Notifications. An errored or exited session raises no notification; the leaf state, the Needs You panel and the footer counts still show it.
- **Needs you** fires when an agent waits on the owner: a question, a permission prompt, a plan approval (today's `AwaitingInput`).
- **Finished** fires when an agent's turn ends: the session goes from Working to Idle, meaning the agent finished its turn and waits for the next prompt. It does not mean the process exited.
- **Background work holds Finished back.** A turn that ends while its subagents, background shells or a scheduled wakeup still run raises nothing; Finished fires once the last of that work is done and the agent is idle (RT-84's background state).
- **Finished waits for RT-84.** Needs you, the removal of the three Tauri-era toasts and the desktop suppression setting ship first; Finished is added once RT-84's background state exists, so it never fires early.
- **No headless sessions.** Headless runs are being removed from the native client (RT-183), so neither notification covers them.
- **Wording.** Title `Needs you` or `Finished`; body the session's label, as today's toasts use it.
- **Build now, sharpen the detection beside it.** Needs you ships on today's status transitions, without waiting for hook status (RT-25).

  The detection gets smarter per agent through RT-74, which copies how herdr and its clients detect state (subagents, keywords) and reuses ops-tower's detection (subagents, dispatch runs, background shells); ops-tower is a prototype of what rustling-tulip absorbs natively and is to be deprecated. When RT-25 makes status hook-driven, the notifications follow with no change of their own.
- **Desktop suppression is a setting.** A setting chooses when the desktop client skips a notification for a session the owner is already looking at. Choices: skip when the client window is focused and the session's pane is on screen (the default); always notify; skip whenever the client window is focused.
- **Mobile gets the same two notifications**, with a setting for whether being active on the desktop suppresses them, on by default. What counts as active on the desktop is itself configurable: recent keyboard or mouse input on the host PC (Windows idle time within a threshold), and the desktop client window being focused. By default both must hold.

## Related items

- RT-25 (hook status): the source that makes Needs you and Finished exact; `docs/plans/hook-status.md` Q3 already rules that `Stop` means Idle.
- RT-84 (background state): supplies "work still running after the turn", which holds Finished back.
- RT-74 (steadier screen-based status): the per-agent detection the notifications rely on until hooks, and for agents without hooks.
- RT-105 (MA7, phone push) and RT-143 (the iPhone PWA re-plan): carry the mobile half and its suppression setting.
- RT-87 and RT-90 (the Wave 5 attention feed, streams and per-stream sounds): the later generalisation; their Needs you and Done streams start from these two kinds.
- `docs/plans/dashboard.md` ("Toasts") reads the shipped tab: the `Needs you` and `Finished` toggles plus the `Skip notifications when` row.

## Decisions (orchestrator, from the code)

- Needs you is derived in the client from the daemon's existing `Attention { AwaitingInput }`. The `Stopped` and `Error` attentions stay on the wire, since the leaf `!` badges and the Needs You "Ended" rows read them, but raise no notification. No protocol change.
- Finished, once RT-84 lands, is a Working → Idle transition the client sees live on `SessionUpdated`; a `Sessions` relist or a reconnect never raises it.
- "The session's pane is on screen" means any pane of the active tab shows the session.
- Sessions marked "Don't count as busy" still notify.
- The old `stopped` and `error` keys in `native-ui.json` are dropped on load.
- Each client window notifies on its own.

## Steps

- [x] **TN.1 Needs you only.** Remove the stopped and errored toasts, their toggles, `notify.rs`'s own-stop bookkeeping and their specs; retitle the awaiting-input toast `Needs you`; one Needs you toggle remains in Settings → Notifications.
- [x] **TN.2 Desktop suppression setting.** The window-focused flag and the active tab's panes decide; the setting's three choices, default "skip when focused and the pane is on screen".
- [x] **TN.3 Finished**, after RT-84: the live Working → Idle transition with no background work, its toggle, and the same suppression.
- [x] **TN.4 Docs.** `docs/native-client.md` Notifications, `docs/architecture.md` (the "attention raises an OS notification" line), `docs/plans/dashboard.md` (reads the shipped tab), and the parity line in `docs/plans/native-client-parity.md`.

The code map with file lines and named tests: `.tmp/map-rt182.md` at the time of planning; re-explore before the first brief if the code has moved.
