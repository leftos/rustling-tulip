# Plan: Semi-autonomous mode toggle

## Context

Power users running long-form tasks (multi-step refactors, batch test fixes,
plan execution) frequently need to nudge Claude after every "I've done X"
pause: "keep going", "next", "yes proceed". The user wants a per-session
toggle in the rustling-tulip app that fires that nudge automatically — flip
it on, walk away, come back to a finished task list (or a real question
that needs human input).

The trigger language the user has in mind is something like:
*"Keep going through your tasks, commit often, don't stop unless you have a
question."* — but the exact phrasing should be editable per session.

This plan covers the MVP only. A "real autonomy" mode (Claude Code Stop
hook with retry budget, idle timeouts, runaway protection) is a follow-up.

## Outcome

After this work:

- Each session has an "Auto-continue" toggle in the SessionPane toolbar.
- When ON, the daemon listens for `attention` events with reason
  `awaiting_input` or `stopped` (graceful stop only — not error/crash) and
  injects the configured instruction as PTY input.
- The instruction text is per-session, editable inline in the toolbar via
  a small popover; defaults to a built-in template.
- A small counter in the toolbar shows "auto-continued N times" so the
  user has a sense of how active the loop has been.
- Toggle state and instruction text persist across daemon restarts via the
  same orphan-meta sidecar that already records pid/members.

Out of scope for v0:

- Stop-hook-based autonomy (would require writing into Claude Code's
  per-project settings; deferred to v1).
- Idle-timeout / runaway protection (cap N at, say, 100 — implemented at
  v0 as a hard daemon-side cap to prevent runaway loops).
- Multi-instruction libraries / shared snippets across sessions.

## Design decisions

### Injection mechanism: send_input on attention events

Instead of a Claude Code hook (more invasive, harder to test, requires
writing settings files into the repo), the daemon does the work:

1. Subscribe to its own `SessionEvent::Attention` stream.
2. For each event with reason `AwaitingInput` or `Stopped` *and graceful
   exit code (0)*, if the session has `auto_continue.enabled = true`:
   - Wait a short debounce (~750 ms) to let the TUI settle.
   - Write the instruction text + a trailing `\r` into the PTY.
   - Increment `auto_continue.injected_count`.
   - Broadcast a `SessionUpdated` so the UI counter advances.
3. Hard cap at 100 auto-continues per session (configurable later) to
   prevent infinite loops if Claude misinterprets the instruction.

Why daemon-side instead of frontend-side: the user can close the app
window and the auto-continue keeps working. That's the whole point of
having a daemon.

### State: extend SessionRecord + OrphanMeta

```rust
pub struct AutoContinue {
    pub enabled: bool,
    pub instruction: String,
    pub injected_count: u32,
    pub max_injections: u32, // default 100
}
```

Lives on `SessionRecord` and is persisted into `OrphanMeta` so it survives
daemon restarts. New protocol messages:

```rust
ClientMessage::SetAutoContinue { session_id, enabled, instruction, max_injections }
DaemonMessage::AutoContinueState { session_id, state: AutoContinue }
```

`SessionSnapshot` gets a new `auto_continue: AutoContinue` field so the
existing `session_updated` broadcast carries the state through.

### UI: toolbar toggle + popover editor

In `SessionPane.tsx` (and reused by `SessionWindow.tsx`):

- A pill-style toggle next to "Pop out": `Auto-continue: off / on (N)`.
- Click → toggle on/off.
- Right-click or "edit" affordance → small popover with a textarea for
  the instruction and a numeric input for max-injections.
- Disabled (greyed) for headless sessions — they are already one-shot.

Default instruction text constant lives in `apps/tauri-app/src/constants.ts`
next to `CLAUDE_MODELS`:

```typescript
export const DEFAULT_AUTO_CONTINUE_INSTRUCTION =
  "Keep going through your tasks. Commit often. Don't stop unless you have a question.";
```

### Edge cases

- **Skip-permissions off**: Claude prompts for permission per tool use.
  Auto-continue would just answer "yes" forever — that's actually useful,
  but also dangerous. v0 behavior: emit auto-continue on
  `awaiting_input` but log clearly so users see what's happening; surface
  a small warning under the toggle when skip-permissions is off.
- **Session in error state**: do NOT auto-continue. Error means something
  is genuinely broken; the user should look.
- **Headless mode**: toggle is hidden / disabled (one-shot by design).
- **Pop-out window**: toggle works the same — daemon-side state, both
  windows see the same `session_updated` broadcasts.

## Implementation steps

- [ ] **Protocol** (`crates/protocol/src/lib.rs`): add `AutoContinue`
      struct, embed in `SessionSnapshot`, add `SetAutoContinue` and
      `AutoContinueState` messages.
- [ ] **Daemon — record** (`crates/daemon/src/session.rs`): add
      `auto_continue: AutoContinue` field to `SessionRecord` with a
      default. Surface in `snapshot()`.
- [ ] **Daemon — orphan meta** (`crates/daemon/src/orphan.rs`): include
      `auto_continue` in `OrphanMeta` so it round-trips across restarts.
      `insert_orphan` reapplies the saved state.
- [ ] **Daemon — injector loop** (`crates/daemon/src/auto_continue.rs`,
      new): subscribe to the registry's `SessionEvent::Attention`
      broadcast, debounce 750 ms, look up the session's auto-continue
      state, write the instruction to the PTY if enabled and under cap.
      Increment counter via `registry.update`.
- [ ] **Daemon — dispatch** (`crates/daemon/src/server.rs`): handle the
      `SetAutoContinue` client message; persist the change into the
      orphan meta sidecar so it survives restarts.
- [ ] **Frontend — types** (`apps/tauri-app/src/types.ts`): mirror the
      protocol additions.
- [ ] **Frontend — constants**: add `DEFAULT_AUTO_CONTINUE_INSTRUCTION`.
- [ ] **Frontend — toolbar UI** (`apps/tauri-app/src/components/SessionPane.tsx`):
      add the toggle pill and popover editor. Hide for headless mode and
      stopped sessions. Show "(N/max)" counter when enabled.
- [ ] **Verification**: spawn a session with skip-permissions on, ask
      "list 5 facts about cats then list 5 facts about dogs", flip auto-
      continue on, confirm both lists complete and the counter advances.

## Critical files

| Layer | File | New? |
|---|---|---|
| Protocol | `crates/protocol/src/lib.rs` | edit |
| Daemon — record | `crates/daemon/src/session.rs` | edit |
| Daemon — orphan | `crates/daemon/src/orphan.rs` | edit |
| Daemon — injector | `crates/daemon/src/auto_continue.rs` | new |
| Daemon — main | `crates/daemon/src/main.rs` | edit (mod) |
| Daemon — dispatch | `crates/daemon/src/server.rs` | edit |
| Frontend — types | `apps/tauri-app/src/types.ts` | edit |
| Frontend — constants | `apps/tauri-app/src/constants.ts` | edit |
| Frontend — UI | `apps/tauri-app/src/components/SessionPane.tsx` | edit |

## Risks

- **Runaway loops**: Claude might enter a "I'll keep going indefinitely"
  state and never legitimately stop. Mitigation: hard cap (default 100)
  and a daemon-side log/notification when the cap is hit.
- **Permission prompts blanket-yes**: combined with auto-continue,
  destructive operations could occur unattended. Mitigation: don't
  auto-continue on `awaiting_input` when skip-permissions is OFF without
  an explicit "yes I want this" extra checkbox in the popover.
- **PTY input timing**: writing input mid-prompt-render can look ugly or
  garble the TUI. The 750 ms debounce should cover most cases; tune if
  empirically wrong.
