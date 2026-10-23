# Screen status: a Claude session's status from hooks and its screen

RT-281. The daemon stops deriving a Claude session's status from output volume and silence. Hooks stay the first source; a rendered screen, read by rules over its regions, is the second. A quiet screen that still shows a turn in progress stays `Working`.

The code map behind this design: `.tmp/map-rt281.md` at the time of planning (every site that relies on silence, with `path:line`). Re-explore before a brief if the code has moved.

## Owner ruling

- The old Working → Idle setup goes: a session's status comes only from hooks and from screen heuristics more detailed than "is anything being printed", because a quiet screen can still show that work is ongoing.

## What the code does today

- `crates/daemon/src/pty_state.rs` demotes `Working` to `Idle` after `IDLE_AFTER` (2.5 s) of PTY silence, even for a hook-driven session, and `AwaitingInput` to `Idle` once `matches_prompt` stops matching the last 2 KiB of an ANSI-stripped byte tail. A session without hook reports is classified by an output/input byte-volume comparison and a terminal-title heartbeat (`osc_title.rs` `TitleActivity`).
- The daemon has no screen model: no terminal emulator in `crates/daemon` or `crates/tracer`. `alacritty_terminal` is in the lockfile only through `apps/native`.
- Claude Code fires no hook on an Esc interrupt, mid-tool or mid-text (`docs/spikes/hook-status.md` Q4, `docs/spikes/background-tasks.md` case 5); the silence demotion is today's only exit from a stale `Working`.

## Decisions (orchestrator, from the code and the spikes)

1. **Scope.** Claude sessions: interactive agent sessions, and plain shells while `claude` runs in them (hook-driven, or a Claude screen matched). A plain shell that is not running `claude` keeps its own classifier until a prompt-at-cursor read replaces it (booked separately). Codex and Cursor are gone (RT-266).
2. **One grid per session in the daemon**, `alacritty_terminal` (the crate the client renders with, so the daemon reads the screen the owner sees), with no scrollback history, fed from the PTY output broadcast, resized with the PTY, and seeded from the tracer's replay on reattach. A new module `crates/daemon/src/screen.rs` owns it and a region API: the bottom N non-empty lines, the rows after the last full-width `─` rule, the row above the prompt box, the prompt box body, the OSC title.
3. **Claude rules as Rust data in one place**, evaluated by priority over those regions, each with `not` guards and an optional "leave the state as it is" for overlays: working = the spinner line `<glyph> <Verb>…` with or without its `(…)` details (`docs/spikes/chat-mirror-tui.md:61`); awaiting input = a dialog footer (permission, AskUserQuestion, plan approval, folder trust; lines 75-78); idle = the prompt box `❯` with no dialog footer, or the `⎿  Interrupted · …` row with no spinner; overlays (Rewind, detailed transcript, `/model`, `/btw`, the Heads up notice) leave the state as it is; the OSC title glyph (`◐`/`◑` working, `✳` idle) is the lowest-priority rule. This module is the screen engine RT-74 (rule files, user overrides) and RT-51 (chat mirror) build on; there is one engine.
4. **Authority.** A hook event applies at once, as today. A screen-derived change applies only after hysteresis, and only when no hook event arrived during that window (the existing hook-stamp recheck). The screen may promote to `Working` (the spinner line only, which covers `!` commands and turns that fire no `UserPromptSubmit`), demote `Working` and `AwaitingInput` to `Idle`, and raise `AwaitingInput` from a dialog footer.
5. **Hysteresis** (RT-74 owner ruling): leaving `AwaitingInput`, and `Working` → `Idle`, publish after 3 agreeing checks 150 ms apart, capped at 700 ms; entering `AwaitingInput` and entering `Working` publish at once. Checks run after PTY output (debounced) and on the confirmation timer; a clock only confirms what the screen shows, it never infers anything from silence.
6. **Background** stays hook-only (the `Stop` payload); the screen's `· N shell` footer does not set it, and a screen-derived change keeps the flag's existing rule (any status change clears it).
7. **No interrupt hint.** An Esc paints the interrupt row, which is output, which triggers a check; no separate input-side path.
8. **Before the first hook, or with hooks off**, a Claude session is screen-driven only; the volume classifier does not survive for it.
9. **Reattach.** After the replay drains, the restored status is checked against the screen: a visible spinner restores `Working`, a gone dialog clears a restored `AwaitingInput`; when no rule matches, today's restore stands.
10. **Plain shell running claude.** When no Claude screen rule has matched for the hysteresis window (the shell prompt is back, `claude` ended or was killed), a hook-driven shell leaves `hook_driven` and returns to its shell classifier: the replacement for the silence demotion that clears it today (RT-218).

## Steps

Briefs 1 and 2 are additive and land on `main` one by one. Briefs 3 to 6 stack in one worktree branch and land together, so the live tier is never red between them.

- [x] **SS.1 Screen model.** `screen.rs` with the grid and region API, fed and resized beside the watchers; no status change. Red first: a cursor-addressed spinner repaint over one row leaves exactly one spinner row in the bottom lines.
- [x] **SS.2 Claude rules and fixtures.** The rule table and evaluator; fixtures from `docs/spikes/chat-mirror-tui/screens/` and collie's `claude--*.txt` panes (MIT, `X:\dev\_ext\AltanS\collie\web\src\fixtures\panes`). Red first: a fixture-to-state table, including a turn's first seconds with no timer.
- [x] **SS.3 Swap the watchers.** Remove `IDLE_AFTER`, the volume windows, the title heartbeat and the silence demotions for Claude sessions; add the screen-driven write behind hysteresis and the hook-stamp recheck. Red first: a hook-reported `Working` with a quiet screen that still shows the spinner stays `Working` past 10 s; an interrupt row demotes after the hysteresis window; a half-drawn frame raises no `Attention`.
- [x] **SS.4 Reattach.** Decision 9 in `session.rs` `insert_reattached` and `server/recovery.rs`.
- [x] **SS.5 Plain shell running claude.** Decision 10.
- [x] **SS.6 e2e.** fake-claude paints Claude-shaped screens (idle box, spinner line, interrupt row, a dialog footer, title glyphs); `apps/native/tests/e2e_live.rs` waits on them; a live Esc spec; RT-221's idle-wait race goes with it.
- [ ] **SS.7 Docs.** CLAUDE.md "Status detection" and the orphan-recovery paragraph, `docs/architecture.md` (the status rows and a Task Index row), `docs/plans/hook-status.md` ("Coexisting with the heuristic"), `docs/spikes/chat-mirror-tui.md:111` (stale), README glossary ("screen engine", "hook-driven").
