# rustling-tulip

Multi-repo Claude Code wrapper. Native desktop client + long-lived Rust daemon that orchestrates many parallel `claude` sessions across repos and across coordinated multi-repo "workspaces".

## Layout

```
crates/
  protocol/     - shared message types (daemon <-> client)
  daemon/       - long-lived background daemon (WS server, PTY pool, registry)
  daemon-client/ - client-side daemon supervision shared by the clients
apps/
  native/       - native desktop client (GPUI + alacritty_terminal)
tools/
  e2e/fake-claude/ - fake `claude` CLI shim for the native e2e tier
docs/
  architecture.md - components, what the product does, task index
  native-client.md - the native client's design decisions
  plans/MAIN.md - the plan snapshot, generated from Linear: open work in waves
  plans/        - design files for open items (archive/ holds older finished designs)
```

## Build

```powershell
# daemon + tracer + native client, then sweep target/ output older than 14 days
.\rt.ps1 build

# build, then run the native client
.\rt.ps1
```

```powershell
# tests, run with cargo-nextest (`.\rt.ps1 setup` installs it)
.\rt.ps1 test
```

`.\rt.ps1 help` lists the other commands.

## Run

The native client auto-starts the daemon if it isn't already running. Daemon listens on a random loopback port; connection details are written to `%APPDATA%\leftos\rustling-tulip\config\daemon.json` (override the directory with `RUSTLING_TULIP_CONFIG_DIR`).

## Docs

Start with [docs/architecture.md](docs/architecture.md) for how the pieces fit and what the product does, and [docs/plans/MAIN.md](docs/plans/MAIN.md) for a snapshot of the open work. The plan itself lives in Linear, synced two ways with this repo's GitHub issues.

## Glossary

- **Account store**: cswap's folder of saved logins (`~/.claude-swap-backup` on Windows), read and written by both cswap and the daemon.
- **Active account**: the Claude login in `.credentials.json` that every Anthropic Claude session on the machine uses; a switch changes it for all of them at once.
- **Agent conversation id**: the id a Codex or Cursor session's own CLI resumes with, recorded on the session and its history entry (`agent_conversation_id`); Claude's stays in `claude_session_id`.
- **Agent harness**: a terminal coding-agent CLI the daemon can spawn as a session's runtime (Claude Code, Codex, Cursor, and the ones `docs/plans/agent-harnesses.md` adds); its Tier A / B / C says whether its conversation id is chosen before launch, pre-created, or found after launch.
- **Agent page / Agents category**: the Settings modal's `Agents` group row, a label with no page of its own, and the Claude, Codex and Cursor pages under it that hold each runtime's spawn defaults, such as Claude's approval mode and Codex's hooks toggle.
- **Agent skill pack**: the `rt-*` skills and the `rt-worker` agent under `agent-pack/`, installed into `~/.claude` so an agent can fan work out, review it and merge it.
- **Attention feed**: the daemon's one stream of items that want the user (Needs you, Hand-off ready, Progress, Done, Fleet), shared by every client and the phone push, resolved everywhere at once; planned (RT-87, `docs/plans/off-orca.md`).
- **Auto-switch**: moving the active account to another one when its binding usage window reaches the threshold, run by the daemon or by `cswap auto`, never both.
- **Background state**: a session whose turn ended while its subagents, background shells or a scheduled wakeup still run; it never counts as finished (RT-84).
- **Binding window**: whichever of an account's usage windows (5-hour, 7-day, per-model) is fullest; its percentage decides auto-switch.
- **Brief**: a worker's instructions: tree root, files, the change, and a proving command.
- **Busy-excluded**: a per-session daemon flag (`busy_excluded` on the snapshot, sidecar and history entry) marking a session the user does not want counted as busy; set by `SetSessionBusyExcluded`, optionally carried to a duplicate and kept through reattach and recovery.
- **CA.0, CA.1, …**: step ids in `docs/plans/accounts.md` (Claude accounts), distinct from `agent-cli.md`'s AC steps.
- **Chat mirror**: the planned chat view of a Claude session, built from its transcript and hook prompts while its terminal keeps running, answering prompts by typing keys into the PTY (RT-51).
- **Claim lease**: a short-lived marker in the account store saying one process is fetching an account's usage, so another does not fetch it too.
- **Claude provider**: the service a Claude session's `claude` CLI talks to, Anthropic (the default) or DeepSeek, chosen per spawn and kept by every respawn; not an agent.
- **claude-swap / cswap**: the user's multi-account switcher for Claude Code (`github.com/leftos/claude-swap`), whose store and rules the daemon's accounts module shares; see `docs/plans/accounts.md`.
- **Clear and continue**: the planned one-click session action that types `/clear`, waits for the clear to finish, then types `/nextup`; refused while background work runs (RT-91).
- **Closing mark**: the native client's flag on a tab it expects the daemon to remove (closing it, closing its last pane, moving its only pane out), read by placement and Undo until the removal arrives; a refused move clears it, and closing a last pane while a split or move for that tab is in flight does not set it.
- **Connection origin**: whether a client connected on the local port (`Local`) or through the LAN listener or relay (`Remote`); account administration is local only.
- **Detached**: the native client's last Repos-view container, drawn only while it holds sessions, for sessions with no container of their own (their repo or workspace was removed, or their repo is now a member of a workspace); they keep running and streaming.
- **Detached stop all**: the two-click `Stop all` chip in an expanded Detached container's banner: the first click arms `Confirm: stop all`, the second stops every leaf still running, and Esc disarms; it is absent once every leaf is stopped or the container, sidebar or Sessions panel is folded away; see `docs/native-client.md` ("Repos and the no-repos states").
- **Dictation box**: the planned overlay that collects speech recognized on the client (desktop or phone) for one session, for review before it is inserted as a paste or sent; see `docs/plans/dictation.md`.
- **DK.1, DK.13**: step ids of the finished DeepSeek Claude sessions design (DK steps), named in commit messages; its decisions are in `docs/architecture.md` ("Claude provider") and `docs/native-client.md`, distinct from dispatch-follow's `DF` steps and `DS` leaf tag.
- **Drawn-text probe**: the native client's debug-build record of the text each `ellipsis.rs` helper drew, keyed by element id, switched on by `ellipsis::enable_probe()` in specs and read with `ellipsis::drawn_text(id)`; see `docs/native-client.md` ("Truncating text with `…`").
- **E2E tier / smoke tier**: the native client's opt-in test layers above the in-process UI specs. The e2e tier drives the client in-process against a real daemon isolated under `.tmp/`; the smoke tier launches the real exe in a cloaked window. They run through `rt.ps1 native-e2e` and `native-smoke`.
- **Env reference**: an environment-row value written `${env:NAME}`, resolved from the daemon's environment, then the user and system environment, at spawn; only the reference is stored and echoed; see Secret reference.
- **ES.1, ES.9**: step ids of the finished secrets-in-env-rows design (ES sealing steps), named in commit messages; its decisions are in `docs/env-secrets.md`.
- **Fan-out**: an agent splitting a task into independent parts, each run by a worker in its own worktree.
- **Fan-out ledger**: the parent's event log for one fan-out, in the repo's git common dir.
- **Feature marker**: `branch: feat/<name>` in a wave's Linear project content; every item in that project lands on the `feat/<name>` branch instead of `main` (user-level `nextup` §3, "Feature branches").
- **Feature PR**: the draft pull request from a marker's `feat/<name>` into `main`, opened with the first item under the marker and merged with `--rebase` by `/ship` once every item under the marker has landed.
- **First-connect chooser**: the dialog a client with no saved tab layout gets when it connects, offering start empty, open all active sessions in a chosen arrangement, adopt a previous layout or copy another client's; see `docs/native-client.md` ("Tab layouts").
- **Fix round**: review findings sent back to the worker that made the change.
- **Folder banner**: the line under an expanded DIR or SH container in the native sidebar, naming the container's folder on one truncated line with an `Add repo` link and, once the daemon finds a `.code-workspace` there, an `Add workspace` link; see `docs/native-client.md` ("Repos and the no-repos states").
- **Folder-only entry**: a history entry with no spawn config (imported from a tracer log, or a standalone session); it is recovered by running claude in its folder with its `--add-dir` set, never by checking out a branch.
- **Forwarder**: the daemon's per-connection task that streams one session's PTY output to one client; `LoadScrollback` replaces it.
- **Hand-off ready**: a session whose orchestrator ended its turn with `Run /clear, then /nextup to continue.` and has no background work left (RT-85).
- **Hibernation**: stopping an idle, finished agent's process and resuming its conversation when the session is opened again; a planned idea (RT-73, `docs/plans/borrowed-ideas.md`).
- **Hook shim**: the `rt-tracer.exe hook` subcommand an injected Claude Code hook runs; it reads the event on stdin, re-reads `daemon.json` on every call for the current port and token, and POSTs the body to the daemon's loopback `/hook/{session_id}` route with an `X-RT-Hook-Time` stamp, exiting 0 whatever happens.
- **Hook-driven**: a session's runtime-only flag, set by the first mapped hook event that passes the ordering check; while it is set, hook reports own the session's promotions and the PTY heuristic only demotes. A rebuilt record starts false.
- **Hook-driven status**: a session's status taken from Claude Code hook events rather than the PTY heuristic, from the first mapped event that passes the ordering check onward; the heuristic keeps only its two silence demotions (`docs/plans/hook-status.md`).
- **HS.1, SA.1, DB.1, SS.1, DF.1, FT.1, DI.1**: step ids in the subplans `hook-status.md`, `spoken-alerts.md` (SA alerts, DB Dashboard), `subagent-streams.md`, `dispatch-follow.md`, `remote-file-transfer.md` and `dictation.md` under `docs/plans/`.
- **Hysteresis (status)**: holding a status change until several checks agree, so a half-drawn screen does not flip a session's status and back; a planned idea for the screen-based status (RT-74).
- **Landed**: an issue's state once its commit is on `main` (or on its feature branch), waiting for the owner's acknowledgement, which moves it to Done.
- **Last-launch summary**: the muted second line of a repo's or workspace's container row in the native sidebar, naming the stored spawn config's agent, run mode, target, model, authority (or `trusted`) and env-row count, absent parts omitted; the row's tooltip reads `Last launch: <summary>`; see `docs/native-client.md` ("Repos and the no-repos states").
- **Launch last**: replaying a repo's or workspace's stored spawn config into the current tab, from the row's ▶ (shown on hover or keyboard focus), a double-click on its row, or the container menu's `Launch last again ▸`; a worktree config waits for a fresh branch name, a trusted one opens the spawn dialog after a warning, and one with no stored config opens it locked to the container; see `docs/native-client.md` ("Repos and the no-repos states").
- **Linked workspace file**: the `.code-workspace` path a workspace stores as `linked_vscode_workspace`, whose folders the workspace creator and the detected prompt turn into members; a workspace created with Create & watch (`watch_vscode_workspace`) follows it as it changes.
- **Locked key**: a routing variable choosing the endpoint or the credential, which a session's environment rows may not set; a spawn that tries is refused.
- **MA1, MA2, …**: item ids in `docs/plans/mobile-app.md`, the iOS and Android app's phases (distinct from the macOS plan's M0–M4).
- **Native client**: the GPUI + `alacritty_terminal` desktop client under `apps/native`; see `docs/native-client.md`.
- **Needs You**: the native client's rail panel listing every session waiting on the user, longest wait first; see `docs/native-client.md`.
- **One-shot answer**: a spawn choice that belongs to one launch and is dropped from a captured spawn config — the worktree-collision policy (back to `Reuse`) and an in-place target's `checkout_strategy` (back to `None`) — so Duplicate, Recover, Restart, Resume and launch last never replay a collision or dirty-checkout choice nobody was shown; `SpawnTarget::with_one_shot_answers_cleared` in `crates/protocol/src/lib.rs`.
- **Own-agent recovery**: recovering a Codex or Cursor history entry as a session of the same CLI in the same folder, resuming its recorded conversation when that CLI can, else a fresh run.
- **P1.1, P4.4b, …**: native client item ids, as phase number and item number (a letter for a split item); the open ones' rulings are in `docs/plans/native-client.md`.
- **Parity checklist**: `docs/plans/native-client-parity.md`, every user-visible Tauri feature with its source file (paths on the `tauri-last` tag); the native client reaches parity when it is all ticked.
- **Peer identity**: the tailnet peer's whois identity (node, user and stable id) attached to a connection that arrives on the tailnet listener.
- **Pending create**: the native client's ordered queue of request ids for tabs it asked to create and will make active on arrival; a refusing `Error` or a new `Welcome` removes ids from it.
- **Petal**: the approved refreshed look for the native client (graphite ground, tulip-coral accent, status shapes), its settled design in `docs/native-client.md` ("Petal chrome", "Theme and fonts").
- **PF, AT, SV, RD, AR, OT**: idea ids in `docs/plans/off-orca.md` (files across machines, attention streams, session views, remote desktop, artifacts by account, absorbing ops-tower).
- **Plan snapshot**: `docs/plans/MAIN.md`, generated from the Linear team's issues, one line an item, by wave in working order; never edited by hand.
- **Progress update**: an attention-feed item for a stop while work continues, or a summary of work in progress; shown in the feed, never pushed to the phone.
- **Provider table**: the daemon's built-in, not user-editable list of the environment variables each non-Anthropic provider sets, removes and locks (`crates/daemon/src/agents/providers.rs`).
- **Quarantine**: an account whose refresh token was rejected (`invalid_grant`), skipped until it is logged in again.
- **RA.1, CR.0**: step ids of the finished own-agent recovery design (RA recovery steps, CR the Cursor spike), named in commit messages.
- **Re-ask**: the native client resending a queued in-place spawn, unchanged, when its checkout prompt's turn comes, so the daemon answers with current numbers instead of the stale prompt being shown.
- **Reached tests**: the tests a change's diff reaches through the test map, as one nextest filterset; `.\rt.ps1 test -Reached` runs them, and a central or unmapped file reaches the full suite instead.
- **Recovery**: respawning a session from its history entry: a Claude entry with `claude --resume <conversation id>`, as a Claude session or as a shell that types the command; a Codex or Cursor entry by own-agent recovery.
- **Relay**: the planned `crates/relay` service on a small VPS that joins a daemon's outbound connection to the phone's, forwarding bytes it cannot read; the pinned TLS runs end to end through it.
- **Ring (keyboard)**: a native dialog's ordered, wrapping list of stops that Tab and Shift-Tab walk; not the drawn focus ring, the outline around the focused control; see `docs/native-client.md` ("Keyboard reachability").
- **Rollout file**: Codex's per-thread transcript under `$CODEX_HOME/sessions/YYYY/MM/DD/`, whose name carries the thread id.
- **Routing variables**: the `ANTHROPIC_*` and `CLAUDE_*` environment variables that point Claude Code at another endpoint, credential and model; built at each spawn, never stored.
- **Safe option**: the non-destructive button a confirm dialog opens on — Cancel, Keep running, Ignore, or a pane-only close; see `docs/native-client.md` ("Keyboard reachability").
- **Sealing**: the daemon's replacing of a secret row's literal value with a secret reference before the spawn config is captured, at spawn and, for older files, at startup; see `docs/env-secrets.md`.
- **Secret reference**: an environment-row value written `${secret:<id>}`, standing for a value the daemon saved in Windows Credential Manager; resolved at spawn, stored and echoed in place of the value.
- **Secret row**: an environment row whose key has a `KEY`, `TOKEN`, `SECRET` or `PASSWORD` segment or which the dialog's Secret toggle marks.
- **Session history**: the daemon's record of ended sessions, one `history/<id>.json` per session under the config dir, kept 7 days; see `docs/architecture.md` (Session history and recovery) and `docs/native-client.md` (Session recovery).
- **Shared client core**: the planned Rust crate holding the pinned-TLS connect, host profiles and pairing, used by both the native client's remote mode and the mobile app.
- **Spike**: throwaway code that proves an approach works, kept outside the main build (`spikes/`) and deleted once its code is ported.
- **Status glyph**: the shape-and-colour mark for a session's status in the native client (working arc, asking diamond, waiting ring, idle dot); see `docs/native-client.md`.
- **`status_since`**: the daemon's stamp of a session's last status change, on its snapshot and kept across a reattach; Needs You measures waits from it.
- **Stop (keyboard)**: a control Tab and Shift-Tab land on in a native dialog; see `docs/native-client.md` ("Keyboard reachability").
- **Subplan**: a `docs/plans/*.md` design file holding the design and rulings for open items, linked from their issues and deleted once nothing open links it and its durable text is promoted into `docs/`.
- **Summary view**: the planned third view of a session beside terminal and chat, one summarizer line per stretch of work (RT-111).
- **Suspend rig**: a driver that freezes only the process under test, for longer than the bound under test, then resumes it, so a starvation-only failure goes red without loading the machine; one long freeze reproduces while repeated short ones do not, since the process wins a run window.
- **Swatch line**: one row of colour swatches in the native client's Appearance views that is a single keyboard stop; Left and Right move along it.
- **Tailnet**: the user's Tailscale network; the daemon's tailnet listener serves TLS on the host's tailnet addresses (port 8787) with a `tailscale cert` certificate, and `crates/tailnet` wraps the `tailscale` CLI; see `docs/plans/remote-access.md`.
- **tauri branch**: the branch holding the earlier, retired Tauri desktop app, cut from the `tauri-last` tag and kept as history. Nothing is fixed or built there, and daemon fixes are not cherry-picked to it.
- **Test map**: `tools/test-map.json`, which maps source paths to the nextest filterset that tests them (`rows`), and lists the `central` files that run the full suite and the paths to `ignore`; `.\rt.ps1 test -Validate` checks it.
- **Test mode**: whether each landed change runs the full suite (`stable`) or only the fast checks and its reached tests (`unstable`, this repo's mode until the stable milestone, Wave 3's cutover); the full suite still runs before a push and before the release step.
- **test-release**: the Cargo profile tests build with (`cargo nextest run --cargo-profile test-release`; nextest's own `--profile` is not the build profile): release optimisation without LTO, with 16 codegen units and incremental builds, so an edit rebuilds in seconds.
- **Tracer-log import**: the startup pass that rebuilds history entries from `logs/tracer-<id>.log` files for sessions that ended before history existed.
- **Tracking issue**: the issue `Merge feat/<name> (#N)`, last in a feature marker's project, that stands for its feature PR until the PR merges.
- **Unexpected end**: a session whose tracer was lost (killed or crashed) rather than one that exited or was closed; these are pre-ticked for recovery.
- **Unseen turn**: an agent session whose turn ended while this client wasn't showing it focused; it shows the waiting ring until focused.
- **Untouched (picked) layout**: a tab whose layout type was last chosen by a pick (Rearrange or the first-connect chooser) and has not been hand-resized (a divider drag) or hand-arranged since; the daemon rebuilds that type as panes come and go, a pick of the automatic grid also remembers the pane area's aspect and follows it when the window is resized (that re-fit keeps the tab's empty panes, where a fixed pick just keeps its count), and the per-client flag is `TabEntry.picked_layout`; see `docs/native-client.md` ("Tab layouts").
- **Warm-up gate**: auto-switch making no decision until every usable account has been polled since it started.
- **Wave**: a release-sized bundle of plan items that share owning files, so one implementer reads those files once and one review covers the bundle; each wave is a Linear project whose content names its files, review and verification.
- **Worker**: a subagent or rustling-tulip session running one brief in one worktree.
- **Worktree seeding**: preparing a new worktree before its agent starts by copying the ignored files a repo lists, linking shared folders and running its setup script; a planned idea (RT-71, `docs/plans/borrowed-ideas.md`).
