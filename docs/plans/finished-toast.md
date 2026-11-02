# False "Finished" toasts for Claude sessions: findings

Observed on a live install (v0.7.0-alpha client and daemon, Claude Code v2.1.316, Windows). Session contents are scrubbed; spinner examples are paraphrased with the same shape.

## Symptom

"Finished" toasts arrive while Claude is still mid-turn (thinking, running tools, printing messages) and while a background shell is running.

## How the toast is decided (client)

`apps/native/src/notify.rs` `finished()`: a toast fires on any `SessionUpdated` that goes `Working → Idle` (or `Idle+background → Idle`), with `background == false` and the mode not PlainShell. The client is doing what it is told. The bug is that the daemon publishes a false `Idle` during a turn.

## Root cause 1 (confirmed live): the spinner regex misses a task-subject spinner that has a phase

When a todo-list item is in progress, Claude Code replaces the spinner's verb with the task's subject and adds the phase:

```
✶ Fix the build steps 3-4: compile, live run, submit… · thinking (5m 24s · ↓ 9.7k tokens · thinking with high effort)
```

`crates/agents/claude/src/rules.rs` `SPINNER` has two alternatives:
- `\p{Lu}[\p{L}'’-]*…(?:\s+·\s+[\p{L} ]+?)?(?:\s+\(.*)?`: a single capitalised word. It allows the ` · phase` part, but a multi-word subject never matches.
- `\S[^…]*…\s+\(.*`: multi-word, but it needs ` (` directly after the `…`, with no room for ` · thinking`.

The line above therefore matches neither. `turn_spinner` reports no match, `idle_prompt_box` (the `❯` box is always drawn) wins, and the session reads as Idle.

Live evidence: `rustling-tulipd screen <id>` on a session 5m24s into a turn printed `turn_spinner … no match` and `winner: idle_prompt_box (idle)`, three samples in a row, while the title glyph cycled ◐/◑ (`title_working` matched). The sidecar status was `idle`. A session using a plain verb spinner (`* Architecting… · tool use (3m 2s …)`) read Working correctly.

How this produces toasts: each hook `PostToolUse` or `UserPromptSubmit` sets Working. The screen then demotes the session to Idle after the Working→Idle hysteresis (`pty_state.rs` `follow_screen`: 3 agreeing checks ≥150 ms apart, or 700 ms), and every demotion is one toast. The activity log (`activity/session-<id>.jsonl`) shows the signature: `working` at 18:19:07.254, `idle` at 18:19:07.664, 410 ms apart. So any turn running under a todo item toasts after nearly every tool call.

Fix: add the optional phase to the second alternative:

```rust
const SPINNER: &str =
    r"^\s*[·✢*✶✻✽✳]\s+(?:\p{Lu}[\p{L}'’-]*…(?:\s+·\s+[\p{L} ]+?)?(?:\s+\(.*)?|\S[^…]*…(?:\s+·\s+[\p{L} ]+?)?\s+\(.*)\s*$";
```

Add these to `spinner_matches_a_task_subject_and_keeps_its_non_matches`:
- should match: `"✶ Fix the build steps 3-4: compile, live run, submit… · thinking (5m 24s · ↓ 9.7k tokens · thinking with high effort)"` and `"✻ Fix the build… · tool use (40s · ↓ 2k tokens)"`
- should not match: `"✽ Settings form: validate the fields… · thinking"` (no details, so not a spinner)

Also update the doc comment above `SPINNER`.

## Root cause 2 (structural, recommended): the screen may end a hook-driven turn

A hook-driven Claude session gets a reliable turn end from `Stop`/`StopFailure`, yet `follow_screen` still lets a screen reading demote `Working → Idle`. Any future change to Claude Code's spinner text, or a spinner pushed more than 24 rows above the prompt box (the `AbovePromptBox { rows: 24 }` region; a long todo list or subagent panel can do this), brings the same bug back. Suggestion: for a `hook_driven` record whose last status came from a hook, the screen should not publish `Working → Idle`. Only a hook `Stop`, an interrupt (`idle_interrupted`) or the agent's exit should end the turn. The screen would still raise `AwaitingInput` and promote to Working. This needs a design call against `docs/plans/screen-status.md`, which currently says screen writes apply in hook-driven mode too.

## Root cause 3 (defensive, unverified that Claude sends it): a subagent `Stop` ends the main turn

`crates/agents/api/src/hook_vocabulary.rs` `map_signal` maps `Stop | StopFailure → Idle` whether or not the payload carries `agent_id`. `crates/daemon/src/hook_status.rs` `apply_report` only filters subagent **Working**:

```rust
.filter(|status| !(report.subagent && *status == SessionStatus::Working))
```

If Claude Code ever delivers a `Stop` with `agent_id` (the existing test `a_subagent_stop_leaves_background_alone` assumes it can), a subagent finishing drops a working session to Idle, with no background flag, and a toast fires. Fix: filter `Working | Idle` for subagent events, and add a test that a `Stop`/`StopFailure` carrying `agent_id` leaves a Working record Working (still stamping `last_hook_time`). I did not capture a real payload, so check what Claude Code sends first (log one hook body).

## Tooling gaps hit while diagnosing

1. **Nothing logs which writer changed a status.** `daemon.log` has no status lines, so a bogus Idle can't be attributed to a hook event, the screen (and which rule), the shell classifier or `hand_shell_back`. Suggested: one `info!` per status change with the writer:
   - hooks: in `apply_signal`/`deliver`, carry `status_change: Option<(from, to)>` on `Applied` and log `event` (`hook_event_name`), `subagent`, `from`, `to`
   - screen: in `pty_state.rs` `publish`, log `reading.rule` and `from`/`to`
   - classifier: `on_plain_shell_transition` and `hand_shell_back`

   Status changes happen a few times per turn, so info level is cheap.
2. **The daemon's default log filter has a dead directive.** `crates/daemon/src/main.rs` `init_tracing` uses `"info,daemon=debug,tower_http=info"`, but the log target is `rustling_tulipd` (every line reads `rustling_tulipd::…`), so `daemon=debug` matches nothing and no daemon `debug!` ever reaches `daemon.log`. Either retarget it to `rustling_tulipd=debug` (check volume first; the screen watcher has debug lines on hot paths) or drop it.
3. **The client logs no toasts.** `native.log` has no line when a Finished/Needs you toast is raised or skipped. A line in `raise_toast` (session id, title, skip reason) would let a user report be matched to the daemon's status lines.

## Product idea from the owner

Herdr (open source) is reported to judge busy/idle better; a comparison is pending (below). The owner considers a session with only a background shell running to be busy, while herdr does not. Make it a user setting, e.g. Notifications → "Background work counts as busy" (default on, today's behaviour: `Idle + background` raises no Finished until the work ends). Off would raise Finished at the main agent's `Stop` even when `background` is set. File it as a Linear add before building.

## Suggested order

1. Spinner regex fix and tests (root cause 1). This alone should remove most false toasts.
2. Status-writer logging, the log filter fix and toast logging (tooling 1–3), each in its own commit.
3. Subagent-Stop guard (root cause 3), after confirming the payload shape.
4. Design call on root cause 2, and a Linear add for the background-busy setting.

## Herdr comparison

Taken from a read of herdr's source (github.com/ogulcancelik/herdr, commit ee5bfb50, 2026-10-29). A research agent did the read; I have not re-checked these citations myself.

**What herdr does for Claude**
- **Screen and title only for state.** Hooks report session identity only (`SessionStart`, skipping events that carry `agent_id`). herdr removed its `Stop`, `PostToolUse`, `SubagentStop`, `UserPromptSubmit`, `PermissionRequest` and `SessionEnd` hooks, because delayed subagent and post-tool hooks were "reviving an idle pane" (CHANGELOG 0.6.7, issue #198; `src/integration/claude_settings.rs` `HOOK_REMOVALS`, `src/detect/mod.rs` `full_lifecycle_hook_authority`).
- **A working title outranks the prompt box.** Rules are in `src/detect/manifests/claude.toml`. `osc_title_working`, regex `^[\x{2800}-\x{28FF}\x{25D0}-\x{25D3}] ` (braille or ◐◑◒◓ then a space), has priority **1100**, above every dialog and the `❯` prompt box (950). `osc_title_idle` (`^✳ `) is 250.
- **The title masks the same regex miss.** herdr's spinner regex `^\s*[*·✢✳✶✻✽]\s+\S.*…(?:\s+\(\d+[smh](?:\s|·)|\s*$)` also fails on the `… · thinking (` line. The title rule keeps the pane Working anyway.
- **More Working rules.** Besides the spinner line, these read Working:
  - the mode line `^\s*[⏸⏵].*esc to interrupt(?:\s|·|$)`
  - `Waiting for N background agents to finish`
  - `· N MCP tasks still running`
- **A lone background shell is idle.** No rule reads the `· N shell` footer (CHANGELOG 0.9.0, #3414).
- **Hysteresis.** Working→Idle waits for 3 rechecks 100 ms apart, or 700 ms at most, but only for the no-match fallback idle. An idle read from a visible signal (the prompt box, or `✳` in the title) publishes at once. There is also a 3 s grace after the agent starts (`src/pane/agent_detection.rs`).
- **Toast rule.** Finished fires on Working or Blocked → Idle, unless the pane is on screen and the window has focus (`src/app/actions.rs`).

**What this means for us**
1. **We have the same regex gap, but nothing to catch it.** Our `title_working` (200) ranks below `idle_prompt_box` (940), and `screen_target` (`pty_state.rs`) never writes a title-only reading. herdr's high-priority title rule is what saves it. It is worth copying as a second guard alongside the regex fix: a working title glyph (◐–◓ or braille) vetoes a screen `Working → Idle`. One open question: herdr's code doesn't show how it handles a stale title after Claude exits. For us, `hand_shell_back` and the agent-exit paths already cover that.
2. **Our background flag adds a second Finished path.** `finished()` also fires on `Idle+background → Idle`, and the `background_shell` rule counts a lone `· 1 shell`. A shell ending, or the footer flickering, therefore raises Finished. That is the owner's "busy" choice, so it is the setting proposed above rather than a bug. The screen-set flag clearing on a footer-less frame is worth checking, though.
3. **Hooks can flip status back and forth.** herdr dropped lifecycle hooks after late hooks revived idle panes. Our ordering check (`last_hook_time`) drops events that are older than the last one applied. It does not protect against a correctly ordered but late `PostToolUse` after `Stop` from an async hook (unverified; worth a log line once status-writer logging exists).
4. **Rules to add:** `esc to interrupt` on the mode line, `Waiting for N background agents to finish` and `· N MCP tasks still running`, each read as Working.
