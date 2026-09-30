# rustling-tulip: main plan
<!-- plan-doc-hygiene: 2026-09-29 7970fec -->

Entry point for anyone, human or agent, continuing this project. **Open work only**, in the order it is worked: [High priority](#high-priority), then the [Waves](#waves) top to bottom, then [Backlog and singles](#backlog-and-singles). The next item is the first line from the top.

- **One line an item**: the action, the files, who asked. Decisions, designs and rulings a brief needs live in the linked subplan, never here.
- **A landed line is deleted in its landing commit**; `git log` is the record of what shipped. An item worked from a subplan also ticks or deletes its entry there, and a native feature ticks the parity lines it delivered in [native-client-parity.md](./native-client-parity.md). A subplan whose last item lands is deleted in that commit, after its durable text is promoted into `docs/`.
- **A review finding the item does not fix** becomes a new line in the wave that shares its files, never a sub-item under landed work.
- **Only open waves stand here**: the commit that lands a wave's last line deletes the wave's heading and renumbers the rest from 1; a new item joins the wave whose files or subject it shares, or opens a new wave before [Backlog and singles](#backlog-and-singles).

Reference: [architecture.md](../architecture.md) (components, what the product does, the task index of files per kind of change) · [native-client.md](../native-client.md) (the client's settled design) · [native-client-parity.md](./native-client-parity.md) (every feature the native client must have) · [completed/](./completed/) (finished designs kept for their rationale) · the README glossary. Subplans: [native-client.md](./native-client.md) (rulings for the open Phase 4–6 items), [remote-file-transfer.md](./remote-file-transfer.md), [hook-status.md](./hook-status.md), [spoken-alerts.md](./spoken-alerts.md), [subagent-streams.md](./subagent-streams.md), [dispatch-follow.md](./dispatch-follow.md), [borrowed-ideas.md](./borrowed-ideas.md), [petal.md](./petal.md), [needs-you.md](./needs-you.md), [conversation-view.md](./conversation-view.md), [deepseek-sessions.md](./deepseek-sessions.md), [mobile-app.md](./mobile-app.md), [macos-compat.md](./macos-compat.md), [reboot-resume.md](./reboot-resume.md), [issue-link.md](./issue-link.md), [agent-cli.md](./agent-cli.md), [agent-skill-pack.md](./agent-skill-pack.md), [recover-own-agent.md](./recover-own-agent.md), [accounts.md](./accounts.md), [agent-cli-update.md](./agent-cli-update.md).

**Gates.** Every build and test runs through the repo's gate from the worktree root: `pwsh tools/gate.ps1 -Log .tmp/<name>.log -TimeoutSeconds <n> -Slot <heavy|light> -- <command>`. The standard set, named by a wave as "the gates": `-- cargo fmt --all --check` (light), `-- cargo clippy --workspace --all-targets --all-features -- -D warnings` (heavy), `-- cargo test -p <crate>` for each crate the wave names (heavy; `-p protocol` includes `v22_compat`), and `-- cargo deny check` when `Cargo.toml` or `Cargo.lock` changed (light). The live tier is `-- pwsh ./rt.ps1 native-e2e` (heavy) and the OS tier `-- pwsh ./rt.ps1 native-smoke` (light). Every item gets a `code-review`; a visible result no test can prove lands on green gates and is listed for a hand-test.

## High priority

- [ ] **Let agents see the running native client** (user; ruled: lands before PT.5 and PT.7): agents working on UI launch the client and look at it rather than guess, and put visual choices to the user as A/B screenshots or mocks rather than prose. The OS tier (`apps/native/tests/smoke_window.rs`) already captures the cloaked window's frame for its pixel probes (`Frame::rgb`); save that frame as a PNG under `.tmp/` on request (a `rt.ps1` verb), with a fixture daemon state (isolated `.tmp/` dirs, a scripted session list) so a shot shows the view being changed; the remaining Wave 1 briefs name it in their proving steps. `smoke_window.rs`, `rt.ps1`.

## Waves

A wave is one release-sized bundle of items sharing owning files, so one implementer reads them once and one review covers the bundle. Wave 1 is the Petal look, which the user ruled lands before the rest of Phase 4; Waves 2–5 finish native client Phase 4 (rulings: [native-client.md](./native-client.md)).

### Wave 1 — The "Petal" look (`palette.rs`, `theme.rs`, `fonts.rs`, `assets/fonts/`, `lib.rs` colours, `activity_bar.rs`, `tab_bar.rs`, `sidebar_view.rs`, `grid_view.rs`, the menus and dialogs)

Review: `code-review`; a person compares the running client with the boards. Verification: the gates with `-p rustling-tulip-native`, the OS tier (its pixel probes read the new colours); hand-test against the canvas.

The user's canvas is https://claude.ai/artifact/B5hxrVTyyR635CQecEpG2N, its boards saved in [canvases/petal/](./canvases/petal/) (briefs and explorers read that copy); every step's rulings and files are in [petal.md](./petal.md). Order: PT.5, PT.6a, PT.7 and PT.9 are free to start (PT.6a and PT.9 build their buttons on `buttons.rs`; PT.5 and PT.7 both edit `lib.rs`, so they run one after the other); PT.6b follows PT.6a; PT.8b needs PT.6b.

- [ ] **PT.5 Rail, tab bar and footer**, with the footer's status counts.
- [ ] **PT.6a Sidebar header, toolbar and containers**, with the Session button's `Ctrl N` hint.
- [ ] **PT.6b Leaves in two densities**: Comfortable (the board's two-line leaf, default) and Compact, set in Settings → General.
- [ ] **PT.7 Pane cards and headers**: 8 px rounded frames, per-session accent borders.
- [ ] **PT.8b Spawn dialog and Settings**.
- [ ] **PT.9 Source control and diffs**.
- [ ] **PT.10 Petal docs**: `docs/native-client.md` "Theme and fonts", CLAUDE.md's `native-ui.json` line, the README glossary.

### Wave 2 — Session labels, menus and busy tracking (`session_menu.rs`, `session_actions.rs`, `sidebar.rs`, `sidebar_view.rs`, `grid_view.rs`)

Review: `code-review`; UI hand-test. Verification: the gates with `-p rustling-tulip-native`, plus `-p protocol -p daemon` for P4.16's field; hand-test the chips, tags and overlay colours.

- [ ] **Claude sessions on DeepSeek** (user; after Wave 1): a Claude provider (Anthropic / DeepSeek) routing a session the way `~/.claude/bin/claude-deepseek.ps1` does, the key from `DEEPSEEK_API_KEY` never stored, carried through Recover, Restart, Resume, Duplicate and Launch last, a `DeepSeek` chip, a default in Spawn defaults; steps DK.1–DK.13, the dialog and chip steps after PT.6b / PT.7 / PT.8b. Design and answered questions: [deepseek-sessions.md](./deepseek-sessions.md).
- [ ] **Recover brings a Codex or Cursor session back as Claude** (found drafting reboot-resume): `plan_recovery`'s `claude_request` swaps the agent options and offers the folder's Claude transcripts. Ruled (user): recover it as its own agent in the same folder, resumed when that CLI can resume, else a fresh run. Ruled (user): research each CLI's resume support from its own docs or source and capture its conversation id at spawn now; the dialog asks for it with an additive `RecoverAs::OwnAgent`. Settled: a fresh run carries no first prompt; a non-Claude entry offers no Claude transcripts; one without spawn settings is disabled with a reason. `crates/daemon/src/history.rs`, `server.rs`, `agents/codex.rs`, `agents/cursor.rs`, `crates/protocol/src/lib.rs`, `apps/native/src/recover.rs`. Steps RA.0 and CR.0 (spikes) to RA.11; RA.0, CR.0 and RA.1 to RA.8 have landed. Design and answered questions: [recover-own-agent.md](./recover-own-agent.md).
- [ ] **Own-agent recovery of an entry whose repo is no longer registered fails at spawn** (found in RA.7): `folder_target` (`crates/daemon/src/history.rs`) falls back to `SpawnTarget::Standalone`, which `spawn_session` refuses for Codex and Cursor ("standalone targets only support plain_shell sessions and interactive Claude", `server.rs` `standalone_supports`). Either let a standalone target run Codex and Cursor in the recorded folder, or disable the row in the Recover dialog with a reason. See [recover-own-agent.md](./recover-own-agent.md).
- [ ] **Codex sessions stop on a trust screen, and on a sandbox chooser the first time** (found in the Recover spike RA.0): Codex shows its trust screen even with `--yolo`, and saves trust under the lower-cased, `subst`-resolved path, so a repo opened through a `subst` drive asks again on every spawn and resume; its first run on Windows also asks which sandbox to set up. Find a per-spawn way to pre-trust the folder and skip the chooser (a `-c` config override or the trust key Codex actually reads). `crates/daemon/src/agents/codex.rs`. See [recover-own-agent.md](./recover-own-agent.md).
- [ ] **Environment rows are stored and sent in plain text** (found drafting deepseek-sessions): a literal value typed into the spawn dialog's environment rows (the `${env:NAME}` references and the key-like-row warning already exist) lands in `meta.json`, `history/<id>.json` and `state.json`'s `last_spawn_config`, and `SpawnConfigReply`, `Repos` / `Workspaces` and `SessionHistory` send it to every client. Steps ES.1–ES.8; ES.3 lands before DK.5. Ruled (user): plan it and draft the options before the DeepSeek steps. Design: [env-secrets.md](./env-secrets.md).
- [ ] **P4.12b Session menu rows**: Duplicate ▸, Move to ▸, Add to current / new tab, Reveal worktree. See [native-client.md](./native-client.md#p412-sessions).
- [ ] **P4.12c Shift-duplicate prefill**: Shift on Duplicate ▸ opens the spawn dialog prefilled from the source session, on P4.4b's target lock, the prefill beating the Spawn defaults; needs P4.12b, which adds the Duplicate ▸ row. `spawn_form.rs`, `spawn_view.rs`, `session_menu.rs`, `session_actions.rs`. See [native-client.md](./native-client.md#p412-sessions).
- [ ] **P4.16 Exclude a session from busy tracking** (user): a daemon-side per-session flag toggled by "Don't count as busy", leaving the title count, the tab badge and the attention highlight. `crates/protocol/src/lib.rs`, `crates/daemon/src/session.rs`, `apps/native/src/tabs.rs`, `window_title.rs`, `session_menu.rs`, `sidebar_view.rs`. See [native-client.md](./native-client.md#p416-exclude-a-session-from-busy-tracking).

### Wave 3 — Tabs, panes and tab state (`tabs.rs`, `tab_bar.rs`, `tab_menu.rs`, `pane_menu.rs`, `spawns.rs`, daemon `tabs.rs` and `state.rs`)

Review: `code-review`; UI hand-test of the shelf. Verification: the gates with `-p rustling-tulip-native -p daemon`, plus `-p protocol` for the `request_id` on errors.

- [ ] **A tab's closing mark goes stale when the daemon keeps the tab** (found in the undo-before-removal fix's review): the mark (`TabsModel::mark_closing_if_last_pane`) is set from the client's grid, so a split racing the close of the original pane, or a `MovePane` the daemon rolls back, leaves the tab in place and marked until reconnect, and an Undo naming it sends `RestoreTab`, refused with "tab already exists". The failed move can be cleared on its `Error` once `MovePane` carries a `request_id` (as the item below needs for `CreateTab` and `MergeTabs`); the split race needs no mark while a split or move for that tab is in flight. Also mark the "Move this pane to a new tab" path (`ExtractToNewTab`, `pane_menu.rs`), which removes a one-pane source tab unmarked. `apps/native/src/tabs.rs`, `pane_menu.rs`, `pane_close_view.rs`.
- [ ] **P4.17 Keep an untouched layout when panes come and go** (user): a per-client "untouched since picked" flag saved with the tab's layout, set by Rearrange ▸ and the first-connect chooser. See [native-client.md](./native-client.md#p417-keep-an-untouched-layout-when-panes-come-and-go).
- [ ] **A rejected `CreateTab` or `MergeTabs` leaves the pending-create armed** (found in P4.13a's review), so the next tab to arrive from anywhere becomes active (`arm_create` callers in `tab_bar.rs` merge, `spawns.rs`, `session_menu.rs`). Clear it on the `Error` / `ActionFailed` that answers the request, which needs a `request_id` on `CreateTab` and `MergeTabs` (additive; `Error` and `ActionFailed` already carry one) and their error paths echoing it.

### Wave 4 — Repos, containers and presets (`sidebar.rs`, `sidebar_view.rs`, `spawn_view.rs`, new creator and preset modules, daemon `presets.rs` and `vscode.rs`)

Review: `code-review`; UI hand-test. Verification: the gates with `-p rustling-tulip-native`; hand-test the real folder and `.code-workspace` pickers and a preset launch.

- [ ] **P4.10 Repos and workspaces**: + Repo, + Workspace, remove with its confirm, the workspace creator, the "VS Code workspace detected" prompt, the DIR / SH container actions and the no-repos states. See [native-client.md](./native-client.md#p410-repos-and-workspaces).
- [ ] **P4.11 Containers**: Launch last, the full container menu, the keyboard fold and last-launch summary, the Detached banner and stop all, "Resume all (N)", the spawn dialog's container entry points. See [native-client.md](./native-client.md#p411-containers).
- [ ] **P4.15a Preset wizard: sources and variables**. See [native-client.md](./native-client.md#p415-preset-wizard).
- [ ] **P4.15b Preset wizard: preview and launching**, with sticky progress and failure toasts; needs P4.11. See [native-client.md](./native-client.md#p415-preset-wizard).

### Wave 5 — Shared form controls (`settings_view.rs`, `appearance_view.rs`, `diff_tab_view.rs`, `shell_view.rs`, `spawn_view.rs`, `lib.rs`)

Review: `code-review`; UI hand-test of each converted form. Verification: the gates with `-p rustling-tulip-native` (the existing `ui_settings`, `ui_appearance_editor`, `ui_spawn_dialog`, `ui_shell`, `ui_diff_tab` specs hold).

- [ ] **One shared checkbox-row helper**: five near-identical copies exist (`settings_view.rs` `toggle`, `appearance_view.rs` bold row, `diff_tab_view.rs`, `shell_view.rs`, `spawn_view.rs` `checkbox`), and `section` exists three times with different styling (`settings_view.rs`, `appearance_view.rs`, `lib.rs`).
- [ ] **Keyboard focus ring through the Settings Appearance tab's body** (swatches, font list, size steppers), which only the mouse reaches (found in P4.2a). `appearance_view.rs`, `settings_view.rs`.

### Wave 6 — Hook-reported agent status (`crates/daemon/src/hook_status.rs`, `pty_state.rs`, `server.rs`, `agents/claude.rs`, `crates/tracer/src/hook.rs`)

Review: `code-review`. Verification: the gates with `-p protocol -p daemon -p tracer`, then the live tier (fake-claude runs the injected hooks, HS.9); HS.0 is a spike with a real `claude`, recorded by hand.

- [ ] **Hook-reported agent status** (borrowed from VelaTerm): Claude Code hooks injected through `--settings` report working / asking / waiting / idle, with the `pty_state.rs` heuristic as the fallback; steps HS.0 (a spike of the CLI behaviour it relies on) through HS.11. Design, answered questions and steps: [hook-status.md](./hook-status.md).

### Wave 7 — Alerts, Dashboard and Needs You (`crates/daemon/src/summarizer/`, `apps/native/src/alerts.rs`, `speech.rs`, `dashboard.rs`, `dashboard_view.rs`, `settings_view.rs`, `activity_bar.rs`)

Review: `code-review`; a person listens to the voices and looks at the Dashboard. Verification: the gates with `-p protocol -p daemon -p rustling-tulip-native`, plus `cargo deny check` for the key-store crates; hand-test a spoken alert and its toast against a real waiting session. Needs Wave 6.

- [ ] **Spoken alerts when an agent waits** (user): a cheap-model summary (DeepSeek Flash by default, providers and keys chosen in Settings, the daemon calling the provider's Messages API directly: an exception to CLAUDE.md's no-direct-API rule that changes with SA.6) classified `needs_answer` / `working_update` / `done_waiting`, prefixed with the repo's spoken name and spoken with Windows' built-in voices; steps SA.1–SA.11. Phone alerts wait for the mobile app's MA7 push. Design and rulings: [spoken-alerts.md](./spoken-alerts.md).
- [ ] **A local small model as a summarizer provider** (user asked): measure whether a local model served over an OpenAI-compatible endpoint (Ollama or llama.cpp) can replace the paid provider for the alerts' classification and one-line summaries: quality against DeepSeek Flash on recorded transcript tails, latency on CPU and GPU, memory. A spike before SA's provider step. See [spoken-alerts.md](./spoken-alerts.md).
- [ ] **Dashboard view** (user): a tab of cards across all live sessions, fed by the alerts' summarizer, grouped Needs you / Working / Done / Idle, with files changed and an activity timeline; steps DB.1–DB.4 (DB.5 rides the mobile app's MA6). Design: [spoken-alerts.md](./spoken-alerts.md#dashboard-view).
- [ ] **"Needs You" view, hook detail and summaries** (NY.4 needs HS.2, NY.5 needs SA.1, then NY.6 docs): line 2 from `pending_input`, the Answer rows from summaries. Design: [needs-you.md](./needs-you.md).

### Wave 8 — Subagent streams (`crates/daemon/src/subagents.rs`, `server.rs`, `apps/native/src/sidebar.rs`, `sidebar_view.rs`, new `subagent_view.rs`)

Review: `code-review`; UI hand-test with a real Claude session running subagents. Verification: the gates with `-p protocol -p daemon -p rustling-tulip-native`, then the live tier (SS.9).

- [ ] **View subagent streams as clickable sessions** (user): a Claude session's running subagents as foldable read-only rows under its leaf, each opening its transcript in a pane, gone when it finishes; steps SS.1–SS.10 (SS.10 also corrects CLAUDE.md's claim that the daemon tails the session jsonl for tokens and cost). Design: [subagent-streams.md](./subagent-streams.md).

### Wave 9 — Follow dispatch runs (`crates/daemon/src/dispatch_runs.rs`, `agents/claude.rs`, `paths.rs`, `apps/native/src/headless.rs`, `headless_view.rs`, `sidebar.rs`)

Review: `code-review`; UI hand-test with a real dispatch. Verification: the gates with `-p protocol -p daemon -p rustling-tulip-native`, then the live tier (DF.8); DF.9 lands in `~/.claude/skills/dispatch` with that skill's own tests.

- [ ] **Follow DeepSeek dispatches** (user): a run the user-level `dispatch` skill starts registers a live-run file the daemon watches; the client shows it as a read-only `DS` leaf in its worktree's container, opening a headless-style view of its stream, with Stop; steps DF.1–DF.10. Design: [dispatch-follow.md](./dispatch-follow.md).

### Wave 10 — Native client Phase 5: windows and drag-and-drop (`lib.rs`, `grid_view.rs`, `tab_bar.rs`, `sidebar_view.rs`, `tabs.rs`)

Review: `code-review`; UI hand-test (multi-window and drag can't be fully specced). Verification: the gates with `-p rustling-tulip-native`, the OS tier.

- [ ] **Phase 5 — windows and drag-and-drop**: pane / tab / session pop-outs as windows of one process, pane drag-and-drop with edge overlays, sidebar and tab drag-to-reorder. Split into P5.1–P5.12 in `native-client.md`'s Phase 5 section, its questions answered. See [native-client.md](./native-client.md#phase-5--windows-and-drag-and-drop).

### Wave 11 — Remote, cutover and the shared client core (new client-core crate, `apps/native/src/net.rs`, `connection.rs`, `links.rs`, `open.rs`, the installer)

Review: `code-review`, and a security read of the pinned-TLS and pairing code. Verification: the gates with `-p rustling-tulip-native -p daemon-client` and the new crate, the live tier; hand-test pairing and a fetch against a second machine, and an install and uninstall.

- [ ] **MA1 Shared client core**: a crate holding the pinned-TLS connect, host profiles and pairing, recovered from `remote.rs` on the `tauri-last` tag, which Phase 6 builds on and the mobile app reuses. See [mobile-app.md](./mobile-app.md).
- [ ] **Phase 6 — remote and cutover**: connection picker, LAN pairing, pinned-TLS tunnel on MA1, autostart recovered from the tag; the installer ships the native client and deletes the `rustling-tulip-daemon` login `Run` value on uninstall; update CLAUDE.md. Ruled (user): split into items and put to the user when Wave 11 comes up, not before. See [native-client.md](./native-client.md#phase-6--remote-and-cutover).
- [ ] **FT.2 Native download sink**: the per-host download folder, `.part` writes and rename, and the open hand-off with the `:line` rule, stripping the `\\?\` prefix from `resolved_path`. Needs Phase 6. See [remote-file-transfer.md](./remote-file-transfer.md).
- [ ] **FT.3 Ctrl-click trigger**: in remote mode, terminal links send `FetchFile` with candidate readings. Needs FT.2. See [remote-file-transfer.md](./remote-file-transfer.md).
- [ ] **FT.4 "Fetch file…" popup**: a path input scoped to the focused session's repo or worktree, with progress, cancel and inline errors. Needs FT.2. See [remote-file-transfer.md](./remote-file-transfer.md).

### Wave 12 — Mobile app (`crates/relay`, the Flutter app, the client core)

Review: `code-review`, and a security read of the relay and per-device credentials. Verification: the gates for the Rust crates; the phone builds and a session answered from the phone by hand.

- [ ] **Mobile app MA2–MA10** (user; after Wave 11): relay, per-device credentials, Flutter shell and pairing, iOS pipeline, monitor and respond, push, full terminal, conversation view, accounts screen, each split into items when it comes up. Phases, rulings and open questions: [mobile-app.md](./mobile-app.md).

### Wave 13 — Claude accounts (new `crates/daemon/src/accounts/`, `server.rs`, `crates/protocol`, `apps/native/src/footer.rs`, new `accounts.rs` and `accounts_view.rs`, `settings_view.rs`, `notify.rs`)

Review: `code-review`, and a security read of the credential handling and the local-only gate. Verification: the gates with `-p protocol -p daemon -p rustling-tulip-native`; the live tier for CA.11; a hand-test switching accounts while two sessions run, with `cswap list` showing the same state.

- [ ] **Built-in claude-swap** (user; `branch: feat/claude-accounts`): the daemon holds several Claude logins in cswap's own store, polls their 5h/7d usage, and switches the machine's login by hand or automatically, from the desktop, a remote client or the phone. The footer chip and a Settings → Accounts tab. Steps CA.0 (spike) to CA.12. Design and rulings: [accounts.md](./accounts.md).

## Backlog and singles

Items that share no files with a wave, what waits on something outside the repo, and ideas that need a design pass before they are brief-sized.

### Build tooling

- [ ] **`proc-macro-error2` future-incompatibility warning**: the chain is `gpui` 0.2.2 → `stacksafe` 0.1.4 → `stacksafe-macro` 0.1.4 → `proc-macro-error2`; `stacksafe` 1.0 drops it and zed's main already uses it, and a `[patch]` can't cross from 0.1 to 1.0. Bump gpui when a release after 0.2.2 ships. `apps/native/Cargo.toml`.
- [ ] **`tracer_protocol` incremental-session note**: every rebuild prints `did not finalize incremental compilation session directory … Access is denied (os error 5)` (harmless; recurs after `cargo clean -p tracer-protocol`; a hand rename seconds later succeeds). Find the brief holder (Defender, a still-mapped `dep-graph.bin` / `query-cache.bin`, or other) and why only this crate.

### macOS

- [ ] **Blocked on a Mac: verify M0–M3 on real hardware**: `cargo build` and `cargo clippy` there, tracer reattach across a daemon restart, `killpg` cleanup of the child tree, and the LaunchAgent plist written and removed by the autostart toggle. See [macos-compat.md](./macos-compat.md).
- [ ] **M4 packaging, signing and notarization**: deferred while distribution is local dev builds only. See [macos-compat.md](./macos-compat.md).

### Agent CLIs

- [ ] **Self-update an agent CLI before launching it** (user): every Claude, Codex or Cursor spawn first tries to update that CLI to its latest version, then launches it. Ruled (user): each CLI's own update command only, a spawn waits up to 20 s, one update per CLI reused for an hour, an `agent_updates` setting, overrides skip it. `crates/daemon/src/agent_update.rs` (new), `server.rs`, `state.rs`, `crates/protocol/src/lib.rs`, the Settings General view. Steps AU.1 (a spike) to AU.8. Design and answered questions: [agent-cli-update.md](./agent-cli-update.md).

### Open questions and blocked items

- [ ] **`--add-dir` hook and settings propagation**: does `claude --add-dir` load hooks and `settings.json` from each additional root, or only from the primary cwd? Not yet verified; it matters for workspace members with their own `CLAUDE.md` or hooks, and Wave 6's HS.0 spike is the natural place to check it.
- [ ] **Auto-update for the native client**: blocked until its installer (Wave 11) and a signed release pipeline exist (no Actions pipeline, no signing cert, no hosted manifest).

### Ideas needing a design pass

Borrowed from Orca and VelaTerm; details, sources, rulings and open questions in [borrowed-ideas.md](./borrowed-ideas.md).

- [ ] **Conversation view (GUI mode)** (user: likes VelaTerm's): a Claude session as a chat with tool cards and permission buttons, switchable with the terminal view; a session remembers its last view. Billing spike: runs on the subscription's usage windows today, personal use only ([gui-mode-billing.md](../spikes/gui-mode-billing.md)). The mobile app's MA9 depends on it. Design and the user's answers, steps CV.0 (spike) to CV.11: [conversation-view.md](./conversation-view.md).
- [ ] **Link a worktree to a GitHub issue or PR at spawn**, through `gh` only; steps GL.1–GL.9. Design: [issue-link.md](./issue-link.md).
- [ ] **Agents that drive rustling-tulip**: a CLI run inside a session to spawn, message and read other sessions, authenticated by a per-session token; steps AC.1–AC.10. Design: [agent-cli.md](./agent-cli.md).
- [ ] **Agent skill pack** (user): skills a user installs so their agents fan work out to subagents in worktrees, then review and merge the results themselves, modelled on this machine's implementer dispatch and worktree protocol; the agent CLI's skill (AC.10) joins it. Replaces a built-in plan-then-execute feature, which the user dropped: an agent handles review and merging best. Steps SP.0 (a spike) to SP.8. Design and answered questions: [agent-skill-pack.md](./agent-skill-pack.md).
- [ ] **Gate dashboard in the client** (user): show which of the machine's heavy and light gate slots are busy, held or free, what each busy slot runs (command, working folder, log, time held) and which gates are waiting, as `~/.claude/tools/gate/gate-dashboard.ps1` serves on localhost today. It reads the slot mutexes, the claim files under `%LOCALAPPDATA%\gate\slots\` and the process list, and prints the same state as JSON with `-Once`. Ruled (user): a rail view beside Sessions, Needs You and Source control, with a busy-slots badge, hidden when no gate state exists; the daemon reads the slot mutexes and claim files itself and pushes the state in a new additive protocol message. Ruled (user): the subplan is drafted now. Design: [gate-dashboard.md](./gate-dashboard.md); steps GD.1–GD.8.
- [ ] **Resume the sessions a reboot killed, on the next start**: a prompt ("Resume all", "Choose…", "Dismiss"), never resuming without asking; builds on session recovery; steps RB.1–RB.8. Design: [reboot-resume.md](./reboot-resume.md).
