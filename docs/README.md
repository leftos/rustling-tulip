# rustling-tulip developer docs

The start page for working on rustling-tulip: a native Windows desktop client (GPUI) and a long-lived Rust daemon that runs many parallel `claude` CLI sessions across repos and multi-repo "workspaces". What the app does for its users is in the [root README](../README.md), and what changed in each release in the [changelog](../CHANGELOG.md); agent instructions are in [CLAUDE.md](../CLAUDE.md).

## Docs

- [architecture.md](architecture.md): the entry point. Components, what the product does, the task index (which files to read, in order, for each kind of change), and non-goals.
- [native-client.md](native-client.md): the native client's settled design decisions, view by view.
- [auto-update.md](auto-update.md): how the native client finds, verifies and installs a published release, and how the daemon is taken over.
- [env-secrets.md](env-secrets.md): how secret env rows are sealed into Windows Credential Manager, and what is and isn't protected.
- [tracer-abi.md](tracer-abi.md): the daemon/tracer ABI, its stability contract and what counts as a bump.
- [release.md](release.md): how a versioned release is cut, from the Linear release pipeline to the GitHub release.
- [remote-hand-checklist.md](remote-hand-checklist.md): the by-hand checks of remote access over a real tailnet, run before a release that touches it.
- [ux-audit.md](ux-audit.md): the UX audit of the retired Tauri app, kept for its findings (its paths resolve on the `tauri` branch).
- [plans/MAIN.md](plans/MAIN.md): the plan snapshot, generated from Linear team RT (open work by wave); the `plans/` folder holds the design files open items link.
- [research/](research/): research notes on outside tools and docs.
- [spikes/](spikes/): write-ups of throwaway experiments.

The plan itself lives in Linear, synced two ways with this repo's GitHub issues.

## Layout

```
crates/
  protocol/     - shared message types (daemon <-> client)
  daemon/       - long-lived background daemon (WS server, PTY pool, registry)
  tracer/       - rt-tracer.exe, the per-session ConPTY supervisor that survives daemon restarts
  tracer-protocol/ - the stable daemon <-> tracer ABI
  daemon-client/ - client-side daemon supervision shared by the clients
  tailnet/      - Tailscale CLI wrapper and the owner rule for tailnet peers
  agents/api/   - the agent plugin trait and its context types
  agents/claude/ - the Claude plugin (argv, hook settings, stream-json parsing), with its fake `claude` CLI shim for the native e2e tier in `fake/`
  agents/registry/ - the compiled-in list of agent plugins
apps/
  native/       - native desktop client (GPUI + alacritty_terminal)
tools/
  e2e/fake-ssh/ - fake `ssh` shim for the native e2e tier's SSH specs
  e2e/fake-tailscale/ - fake `tailscale` CLI shim for the native e2e tier's remote specs
  installer/    - the NSIS installer script
docs/
  README.md     - this page: the docs start page and the glossary
  architecture.md - components, what the product does, task index
  native-client.md - the native client's design decisions
  images/       - the screenshots the root README shows
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

## Glossary

- **Account store**: cswap's folder of saved logins (`~/.claude-swap-backup` on Windows), read and written by both cswap and the daemon.
- **Active account**: the Claude login in `.credentials.json` that every Anthropic Claude session on the machine uses; a switch changes it for all of them at once.
- **Activity track**: the Dashboard's thin two-hour bar per session or dispatch run (working, asking, idle, finished, gate holds as ticks, a hatched gap where the daemon kept no record), drawn from the status-change log; see `docs/plans/dashboard.md`.
- **Agent chip**: a small label a session's agent shows on its sidebar leaf and pane header, such as `DeepSeek`; the daemon evaluates the descriptor's chip rules into `SessionSnapshot.agent_chips` (`{text, tip}`, `agent_api::session_chips`), so a client draws them without reading options.
- **Agent descriptor**: the data a plugin publishes about its agent (`protocol::AgentDescriptor`: id, label, capabilities, texts, the option schema with its Settings and spawn fields, and its title rules), sent to clients as `Agents` so they draw pickers, Settings pages, chips and labels without naming the agent.
- **Agent harness**: a terminal coding-agent CLI the daemon can spawn as a session's runtime (Claude Code today; the ones `docs/plans/agent-harnesses.md` plans return as plugins); its Tier A / B / C says whether its conversation id is chosen before launch, pre-created, or found after launch.
- **Agent in shell**: a session whose PTY child is the user's shell with the agent typed into it, so the pane stays a shell after the agent exits (`SessionRecord::agent_in_shell`).
- **Agent in view**: the subagent whose transcript the TUI shows in place of the main conversation; the prompt's Enter then goes to that agent.
- **Agent note**: the JSON line a shell-hosted agent's tracer writes to its log (`--agent-note`, logged as `rt-tracer agent note`) naming the agent, its options, trust, extra folders and conversation id, so the tracer-log import can rebuild the session as that agent.
- **Agent page / Agents category**: the Settings modal's `Agents` group row, a label with no page of its own, and one page per agent the daemon lists under it, each holding that agent's settings (Claude's approval mode and provider, for one) and its launch command.
- **Agent plugin**: a compiled-in crate under `crates/agents/` that implements `agent_api::AgentPlugin` for one agent (spawn argv, hook settings, headless stream parsing); the daemon reaches it through `agent_registry::plugin`, never by naming the agent.
- **Agent skill pack**: the `rt-*` skills and the `rt-worker` agent under `agent-pack/`, installed into `~/.claude` so an agent can fan work out, review it and merge it.
- **Answer recipe**: the key sequence an agent plugin gives for one answer to one terminal dialog (a digit; Tab, text, Enter; toggles then submit), which the daemon types for a chat answer and checks against the screen after each group of keys; see `docs/plans/conversation-view.md`.
- **Answer rule**: the test that an `Idle` session needs you because its latest summary asks the user something (`NeedsAnswer`) and was made at or after its last status change (`needs_you::is_answer`); the Dashboard's Needs you group uses it.
- **App id (AUMID)**: the Windows application user model id a toast is raised under, which names its sender; the installed native client uses `leftos.RustlingTulip` (`toast_activation::APP_ID`), anything else the PowerShell id.
- **apply-update**: `rustling-tulipd apply-update --from <staging> --to <dir>`, the subcommand the installer runs from the staged daemon to swap the installed files for the staged ones (stop the daemon, rename each file to `.old`, roll back on failure, record `last-result.json`); `crates/daemon/src/cli_apply_update.rs`, `docs/auto-update.md`.
- **Attention feed**: the daemon's one stream of items that want the user (Needs you, Hand-off ready, Progress, Done, Fleet), shared by every client and the phone push, resolved everywhere at once; planned (RT-87, `docs/plans/off-orca.md`).
- **Auth kind**: how a connection authenticates, `Local` (loopback, checked against the `daemon.json` token) or `Tailnet` (the admitted peer's identity, with the presented value read as its device token); it also decides which host-only messages the connection may send (`AuthKind` in `crates/daemon/src/server/connection.rs`).
- **Auto-resume**: the daemon resuming, by itself once its listener is bound, each session a reboot or any other joint death of the daemon and its tracers killed that qualifies (`recovery::auto_resume`); the rest stay in Recover. See `docs/architecture.md` (Session history and recovery).
- **Auto-switch**: moving the active account to another one when its binding usage window reaches the threshold, run by the daemon or by `cswap auto`, never both.
- **Background state**: a session whose turn ended while its subagents, background shells or a scheduled wakeup still run; it never counts as finished (RT-84). A `Stop` hook sets the flag, and so does the screen's footer (a `· N shell(s)` mode-line item or a running-subagent row).
- **Baseline**: the conversations an agent already held for a folder, taken just before a `DiscoverAfterLaunch` session's program starts; only a conversation outside it can be that session's.
- **Binding window**: whichever of an account's usage windows (5-hour, 7-day, per-model) is fullest; its percentage decides auto-switch.
- **Blocked (device)**: a `devices.json` record with `revoked: true`, refused at every pairing path until the owner forgets it (`ForgetDevice`).
- **Boot target**: the `Target` the native client starts on: `native-ui.json`'s `last_target` when it names a saved host of the same kind (dialled as it was saved, the local daemon never started), else this PC; `connection_picker::resolve_boot_target`, `docs/native-client.md`.
- **Bridge**: `rustling-tulipd bridge`, the subcommand an SSH client runs on the host to relay JSON protocol lines between stdin and stdout and the loopback daemon, filling the `Hello` token on the host (`crates/daemon/src/cli_bridge.rs`).
- **Brief**: a worker's instructions: tree root, files, the change, and a proving command.
- **Built-in theme**: a theme compiled into the native client (`BUILT_INS` in `apps/native/src/theme_store.rs`, the values in the generated `builtin_themes.rs`): Contrast, Nord, Catppuccin Mocha, Catppuccin Latte, Rose Pine, Dracula, TokyoNight and One Half Dark; a user theme file cannot take their ids (it gets a `-2` suffix).
- **Busy-excluded**: a per-session daemon flag (`busy_excluded` on the snapshot, sidecar and history entry) marking a session the user does not want counted as busy; set by `SetSessionBusyExcluded`, optionally carried to a duplicate and kept through reattach and recovery.
- **CA.0, CA.1, …**: step ids in `docs/plans/accounts.md` (Claude accounts), distinct from `agent-cli.md`'s AC steps.
- **Capability table**: `apps/native/src/remote_mode.rs` (`Capabilities`), the one place that says what the native client may do on its connection target (open local files, manage the daemon); views ask it rather than the target.
- **Chat entry**: one item of the chat mirror (a user message, an answer, a thinking row, a tool card, a system row, a receipt, a turn end), carried on the wire as `ChatEntry` and upserted by id.
- **Chat mirror**: the planned chat view of a Claude session over its live terminal: history from its transcript, state and dialog options from its screen, live text from the MessageDisplay hook, messages and answers typed into the PTY as keys (RT-51, `docs/plans/conversation-view.md`).
- **Claim**: discovery giving a newly found conversation to the one waiter eligible for it, which sets that session's conversation id; with two or more eligible waiters the id is contested instead.
- **Claim lease**: a short-lived marker in the account store saying one process is fetching an account's usage, so another does not fetch it too.
- **Claude provider**: the service a Claude session's `claude` CLI talks to, Anthropic (the default) or DeepSeek, chosen per spawn and kept by every respawn; not an agent.
- **claude-swap / cswap**: the user's multi-account switcher for Claude Code (`github.com/leftos/claude-swap`), whose store and rules the daemon's accounts module shares; see `docs/plans/accounts.md`.
- **Clear and continue**: the planned one-click session action that types `/clear`, waits for the clear to finish, then types `/nextup`; refused while background work runs (RT-91).
- **Closing mark**: the native client's flag on a tab it expects the daemon to remove (closing it, closing its last pane, moving its only pane out), read by placement and Undo until the removal arrives; a refused move clears it, and closing a last pane while a split or move for that tab is in flight does not set it.
- **Composer state**: what a session's terminal input area can take right now, read from its screen: ready, working, draft (the box holds text), dialog or overlay; the chat mirror refuses a message the state cannot take.
- **Connection origin**: the planned `Welcome` field saying whether a client connected on the loopback listener (`Local`) or the tailnet listener (`Remote`), mapped from the connection's auth kind; account administration is local only (CA.1, `docs/plans/accounts.md`).
- **Container menu**: the native client's right-click menu on a repo's or workspace's sidebar row (spawn, `Launch last again ▸`, `Appearance…`, `Launch preset…`, the open rows, Remove); see `docs/native-client.md`.
- **Contested**: a conversation id that two or more waiting sessions in one folder were each eligible for, so discovery gave it to nobody; kept in `contested/<agent id>.json` for 7 days and never claimed meanwhile (`crates/daemon/src/conversations/contested.rs`).
- **Conversation id**: Claude Code's own id for a session's conversation (`conversation_id` on the snapshot and history entry), which `claude --resume` takes; distinct from the rustling-tulip session id.
- **Daemon proof**: the `GET /proof` challenge-response a `daemon.json` reader asks of the file's loopback port before it trusts it: an HMAC of a random challenge keyed from the file's token (`tracer_protocol::proof`, `docs/tracer-abi.md`); a listener that fails it is treated as no daemon, and is never sent the token or killed.
- **Dashboard**: the native-client tab (opened from the activity rail) over every agent session, in two views: By repo (the default: a need-you band, then one lane per repo with its activity tracks, sessions, Questions and Next up) and By status (one dense row per session with inline answers and a drawer); see `docs/plans/dashboard.md` (RT-28).
- **Dashboard watcher**: a connection whose active tab is the Dashboard (it sent `WatchDashboard { on: true }`), the only kind the daemon sends session facts to (every entry at once, then only the changed ones at most every 2 s, until it sends `on: false` or closes).
- **Day ledger**: the planned `day-ledger.json` in the config dir: per local date, each session's cost and tokens, the commits and the issues moved to Done, for the Dashboard's Today figures.
- **Default theme**: the built-in the native client draws when `native-ui.json` has no `theme` (or null), or when the saved one fails to load: Nord (`theme_store::DEFAULT_ID` and `DEFAULT_THEME`).
- **Detached**: the native client's last Repos-view container, drawn only while it holds sessions, for sessions with no container of their own (their repo or workspace was removed, or their repo is now a member of a workspace); they keep running and streaming.
- **Detached stop all**: the two-click `Stop all` chip in an expanded Detached container's banner: the first click arms `Confirm: stop all`, the second stops every leaf still running, and Esc disarms; it is absent once every leaf is stopped or the container, sidebar or Sessions panel is folded away; see `docs/native-client.md` ("Repos and the no-repos states").
- **Device token**: the value a tailnet client sends in `Hello.auth_token`, checked against the host's approved devices instead of the `daemon.json` token; a device is approved from a loopback client on the host PC (`docs/plans/remote-access.md`, TS.3b). The native client keeps one per host in Windows Credential Manager (`DeviceTokenStore`, `crates/daemon-client/src/device_tokens.rs`).
- **Dialog parser**: an agent plugin's pure function that reads the rendered screen's rows and returns the dialog on it (kind, subject, numbered options, flags), modelled on collie's parsers; the chat mirror's cards come from it.
- **Dictation box**: the planned overlay that collects speech recognized on the client (desktop or phone) for one session, for review before it is inserted as a paste or sent; see `docs/plans/dictation.md`.
- **Dispatch run**: one `claude -p` implementer run the user's `dispatch` skill starts, followed read-only by the daemon from its run folder; see `docs/plans/dispatch-follow.md` (RT-31).
- **DK.1, DK.13**: step ids of the finished DeepSeek Claude sessions design (DK steps), named in commit messages; its decisions are in `docs/architecture.md` ("Claude provider") and `docs/native-client.md`, distinct from dispatch-follow's `DF` steps and `DS` leaf tag.
- **Dock back**: closing a pop-out so its content returns to the main window; it never stops a session or removes a tab, and a pop-out window's close does the same; see `docs/native-client.md` ("Windows and pop-outs").
- **Download root**: the native client's machine-local `remote-files` folder, `daemon_client::remote_files_dir()`, where files fetched from a remote host are saved as `<host>/<repo-name>/<rel-path>`. See `docs/native-client.md` ("Fetched files").
- **Drawn-text probe**: the native client's debug-build record of the text each `ellipsis.rs` helper drew, keyed by element id, switched on by `ellipsis::enable_probe()` in specs and read with `ellipsis::drawn_text(id)`; see `docs/native-client.md` ("Truncating text with `…`").
- **Drop layer**: the native client's transparent layer over the shown tab's grid that finds the pane and edge under a dragged pane and draws its overlay; it stops mouse events only during a drag; see `docs/native-client.md` ("Drag model").
- **Edge overlay**: the tint a pane draws while a dragged pane would drop on it: a half for a split, a strip along the grid's edge for an outer-band split, or the inset centre with a `Swap panes` badge; see `docs/native-client.md` ("Drag model").
- **E2E tier / smoke tier**: the native client's opt-in test layers above the in-process UI specs. The e2e tier drives the client in-process against a real daemon isolated under `.tmp/`; the smoke tier launches the real exe in a cloaked window. They run through `rt.ps1 native-e2e` and `native-smoke`.
- **Env reference**: an environment-row value written `${env:NAME}`, resolved from the daemon's environment, then the user and system environment, at spawn; only the reference is stored and echoed; see Secret reference.
- **Error code**: the snake_case `code` string on a `DaemonMessage::Error`, from the catalogue in `crates/protocol/src/error_codes.rs`; a client branches on it, never on the message text, and keeps an unknown code as an opaque string.
- **ES.1, ES.9**: step ids of the finished secrets-in-env-rows design (ES sealing steps), named in commit messages; its decisions are in `docs/env-secrets.md`.
- **Fan-out**: an agent splitting a task into independent parts, each run by a worker in its own worktree.
- **Fan-out ledger**: the parent's event log for one fan-out, in the repo's git common dir.
- **Feature marker**: `branch: feat/<name>` in a wave's Linear project content; every item in that project lands on the `feat/<name>` branch instead of `main` (user-level `nextup` §3, "Feature branches").
- **Feature PR**: the draft pull request from a marker's `feat/<name>` into `main`, opened with the first item under the marker and merged with `--rebase` by `/ship` once every item under the marker has landed.
- **Fetched copy**: the file a remote-host fetch saved under the download root, which opens in the viewer or an external app in place of the host's file. See `docs/native-client.md` ("Fetched files").
- **Final look**: the last discovery pass for a session still without a conversation id, run inline in `history::record_session_end` before the history entry is built, so an id found late reaches the entry.
- **File reference**: how a prompt names a repo-relative file so an agent reads it (`Agent::file_reference`; Claude's is `@<path>`, the default the bare path); a folder preset's prompt files use it.
- **First-connect chooser**: the dialog a client with no saved tab layout gets when it connects, offering start empty, open all active sessions in a chosen arrangement, adopt a previous layout or copy another client's; see `docs/native-client.md` ("Tab layouts").
- **Fix round**: review findings sent back to the worker that made the change.
- **Folder banner**: the line under an expanded DIR or SH container in the native sidebar, naming the container's folder on one truncated line with an `Add repo` link and, once the daemon finds a `.code-workspace` there, an `Add workspace` link; see `docs/native-client.md` ("Repos and the no-repos states").
- **Folder-only entry**: a history entry with no spawn config (imported from a tracer log, or a standalone session); it is recovered by running claude in its folder with its `--add-dir` set, never by checking out a branch.
- **Footer region**: the screen rows below the last horizontal rule row, where an agent's input box and key hints sit; the screen-rule region kind `after_last_rule` (`ScreenRegion` in `crates/agents/api/src/screen_rules.rs`).
- **Forwarder**: the daemon's per-connection task that streams one session's PTY output to one client; `LoadScrollback` replaces it. When the session's queued output on that connection would pass 4 MiB it drops its live feed and, once the queue drains, resyncs the client with one id-less `Scrollback`.
- **Hand-off ready**: a session whose orchestrator ended its turn with `Run /clear, then /nextup to continue.` and has no background work left (RT-85).
- **Hang guard**: the only wall-clock bound a test may carry, a deadline so long (120 s in-process, 300 s for the live and smoke tiers, `support::live::HANG_GUARD`) that only a hang reaches it, failing with a message starting "hung:"; every other wait is on an event.
- **Heads up notice**: the `✦ Heads up · …` note Claude Code's built-in side checker draws over the input box when it judges the user might miss something, answered by a bare digit (Learn more, Knew this already, Dismiss); see `docs/spikes/chat-mirror-tui.md`.
- **Hibernation**: stopping an idle, finished agent's process and resuming its conversation when the session is opened again; a planned idea (RT-73, `docs/plans/borrowed-ideas.md`).
- **Hook agent**: the agent whose plugin maps the hook events of a plain shell that a recovery typed that agent's resume command into (`SessionRecord.hook_agent`), with the recovered run's options and trust (`hook_options`, `hook_trusted`); daemon-internal, never on the wire `SpawnRequest` or in `SpawnConfig`, and a plain shell without one maps no hook events.
- **Hook shim**: the `rt-tracer.exe hook` subcommand an injected Claude Code hook runs; it reads the event on stdin, re-reads `daemon.json` on every call for the current port and token, and POSTs the body to the daemon's loopback `/hook/{session_id}` route with an `X-RT-Hook-Time` stamp, exiting 0 whatever happens.
- **Hook-driven**: a session whose status is set by Claude Code's hook events, turned on by its first mapped hook event that passes the ordering check; the screen engine still reads its exits and dialogs, and a plain shell leaves it when its prompt returns.
- **Host path**: a path on the connected host, typed by the native client's user where a local picker would open (Add repo, the workspace creator's file, the worktrees root); the host may run another OS, so the client checks it by string rules only (`repo_add.rs`, `HostPathProblem`).
- **Host picker**: the native client's `Connections` modal (`connection_picker_view.rs`): sections for This PC, the saved hosts and the owner's online tailnet peers, one two-line row per host with a state chip, and a `+ Add host` form with a Tailnet | SSH switch whose SSH mode has a **Test connection** step; clicking a row switches the client's target and saves it as `last_target`.
- **Host shutdown**: a Windows logoff, restart or shutdown under way (`GetSystemMetrics(SM_SHUTTINGDOWN)`, `crates/daemon/src/host_shutdown.rs`); a session whose exit arrives then is kept for the next start, which records it as lost so auto-resume picks it up.
- **HS.1, SA.1, DB.1, SS.1, DF.1, FT.1, DI.1**: step ids in the subplans `hook-status.md`, `dashboard.md` (SA summarizer, DB Dashboard), `subagent-streams.md`, `dispatch-follow.md`, `remote-file-transfer.md` and `dictation.md` under `docs/plans/`.
- **Hysteresis (status)**: holding a status change until several checks agree, so a half-drawn screen does not flip a session's status and back; a screen-derived leave of `AwaitingInput` or `Working` to `Idle` publishes after 3 agreeing checks at least 150 ms apart, or the first agreeing check 700 ms after the first (`pty_state.rs`, `CONFIRM_*`).
- **Import revision**: the version of an agent plugin's tracer-log import rules (`Agent::import_rev`; Claude's is 2), stamped on each imported history entry so a later rule change re-imports or upgrades (`Agent::upgrade_import`) the entries made under older rules.
- **Insertion line**: the 2 px accent line a sidebar drag draws at the top or bottom of the group it would drop before or after, chosen by the pointer's half of the group; see `docs/native-client.md` ("Drag model").
- **Keep-list**: the variables every spawned child gets from the daemon's environment whatever its agent (`PATH`, `HOME`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `TEMP`, `TMP`, `TERM`, `LANG`, `LC_ALL`, with `TERM` set to `xterm-256color`), composed in `crates/daemon/src/child_env.rs`; only a headless child starts from it alone.
- **Key holder**: what takes the arrow keys in the TUI: the prompt box, the agents panel or the tasks pill.
- **Key source**: where the Dashboard summarizer finds a provider's API key: stored in Windows Credential Manager, or an environment variable resolved through the process, user and system scopes (`crates/daemon/src/summarizer/keys.rs`).
- **Key state**: what the summarizer reports about a provider's key, never the key itself: `missing`, `stored`, `env_found`, `env_missing`, `rejected` or `unreachable` (Credential Manager is out of reach, as in a daemon started from an SSH login); `rejected` (the provider refused the key) lives in memory only and clears when the key changes.
- **Landed**: an issue's state once its commit is on `main` (or on its feature branch), waiting for the owner's acknowledgement, which moves it to Done.
- **Landed totals**: the Dashboard drawer's files-and-lines figure for a session whose worktrees are all clean, from `git diff --shortstat` over the commits made since the session started (`docs/plans/dashboard.md`, "Recent work").
- **Last message**: the `last_assistant_message` a `Stop` hook reports, trimmed and capped at 8 KiB on the session record, off the wire.
- **Last-launch summary**: the muted second line of a repo's or workspace's container row in the native sidebar, naming the stored spawn config's agent, run mode, target, model, authority (or `trusted`) and env-row count, absent parts omitted; the row's tooltip reads `Last launch: <summary>`; see `docs/native-client.md` ("Repos and the no-repos states").
- **Launch command**: the per-agent host setting (`agent_commands` in `state.json`) naming a Program and Extra arguments that every interactive spawn of that agent uses; headless runs ignore it, and `RUSTLING_TULIP_CLAUDE` still overrides its program.
- **Launch last**: replaying a repo's or workspace's stored spawn config into the current tab, from the row's ▶ (shown on hover or keyboard focus), a double-click on its row, or the container menu's `Launch last again ▸`; a worktree config waits for a fresh branch name, a trusted one opens the spawn dialog after a warning, and one with no stored config opens it locked to the container; see `docs/native-client.md` ("Repos and the no-repos states").
- **Layer**: in `apps/native/src/buttons.rs`, which surface a control sits on, `Base` or `Floating`, deciding its hover and pressed fills (`status_glyph.rs` has an unrelated `Layer`, a tinted SVG layer of a glyph).
- **Lifecycle queue**: the bounded per-session queue that carries a PTY's output chunks, and finally its exit, from the tracer reader to the session's lifecycle task; a queue that closes without the exit item means the tracer was lost.
- **Lifecycle task**: the per-session task (`attach_lifecycle` in `session.rs`) that delivers each chunk from the lifecycle queue to clients and scrollback, answers scrollback snapshot requests, and handles the exit after the last chunk.
- **Linked workspace file**: the `.code-workspace` path a workspace stores as `linked_vscode_workspace`, whose folders the workspace creator and the detected prompt turn into members; a workspace created with Create & watch (`watch_vscode_workspace`) follows it as it changes.
- **Live line**: one `MessageDisplay` hook call's piece of an answer being written (`agent_api::LiveLine`: text, `index`, message and prompt keys), carried to the session's `hook_status::LiveLines` broadcast with no status effect; the chat mirror shows it until the transcript has the answer.
- **Liveness tokens**: the process-name fragments the orphan check accepts for a dead-or-alive test of a sidecar that names no program (`Agent::liveness_tokens`; Claude's are `claude` and `node`).
- **Loader**: a connection that has loaded a session's chat (`LoadChat`) or its summary (`LoadSummary`), or both, and is sent the loaded view's appends (and, for the chat, its screen changes) until it unloads or closes.
- **Locked key**: a routing variable choosing the endpoint or the credential, which a session's environment rows may not set; a spawn that tries is refused.
- **Lost at start**: a session whose sidecar the daemon finds dead at startup with no history entry; it gets a `TracerLost` entry (or its true end from the tracer log) before anything spawns, and is then a candidate for auto-resume.
- **MA1, MA2, …**: item ids in `docs/plans/mobile-app.md`, the iOS and Android app's phases (distinct from the macOS plan's M0–M4).
- **Media batch**: the relative media files (images, scripts, stylesheets and what they name) the native client fetches with a remote `.html` page before opening it, under the download root. See `docs/native-client.md` ("Fetched files").
- **Native client**: the GPUI + `alacritty_terminal` desktop client under `apps/native`; see `docs/native-client.md`.
- **Need-you band**: the band atop the Dashboard's By repo view: the waiting sessions with their asks, the accounts card and Linear Questions counted by repo; see `docs/plans/dashboard.md`.
- **Needs You**: the native client's rail panel listing every session waiting on the user, longest wait first; see `docs/native-client.md`.
- **Needs you group**: the first group of the Dashboard's By status view: the sessions that wait on the user (`AwaitingInput`, or `Idle` when the Answer rule holds), longest wait first; see `docs/plans/dashboard.md` ("Grouping and order").
- **Nested rows**: the Dashboard rows under a Working row (or an open drawer), one per node of the session's agent tree below the main agent, indented with a `└` branch; see `docs/plans/dashboard.md`.
- **Not approved**: the native client's connection state for a tailnet host that answered `Welcome` and then `DevicePairFailed { "device not approved" }` (`State::NotApproved` in `apps/native/src/connection.rs`); the socket stays open, it shows as "not paired", and it redials only on Reconnect now or a target switch.
- **Now line**: the Dashboard row's line saying what a session does this second (the pending prompt, a gate, a subagent it waits on, its oldest open tool call, background work), by the rules in `docs/plans/dashboard.md`.
- **One-shot answer**: a spawn choice that belongs to one launch and is dropped from a captured spawn config — the worktree-collision policy (back to `Reuse`) and an in-place target's `checkout_strategy` (back to `None`) — so Duplicate, Recover, Restart, Resume and launch last never replay a collision or dirty-checkout choice nobody was shown; `SpawnTarget::with_one_shot_answers_cleared` in `crates/protocol/src/lib.rs`.
- **Outer band**: the strip along a pane's side that touches the grid's boundary (12% of the pane); a pane drop there splits the whole tab's root instead of that pane (`OuterLeft` and the other `PaneDropEdge` outer edges); see `docs/native-client.md` ("Drag model").
- **P1.1, P4.4b, …**: native client item ids, as phase number and item number (a letter for a split item); the open ones' rulings are in `docs/plans/native-client.md`.
- **Pairing code**: the one-time code that approves a waiting tailnet device, minted by `MintDevicePairCode` and printed by `rustling-tulipd pair` (`crates/daemon/src/cli_pair.rs`); it works once, for one device.
- **Pairing connection**: a tailnet connection whose `Hello` carried no valid device token; it gets `Welcome` and `DevicePairFailed`, stays open with no layout or session state, and accepts only the `DevicePair*` steps until it is approved and redeems its token (`crates/daemon/src/server/pairing.rs`).
- **Palette token**: a named colour in `apps/native/src/palette.rs`, the built-in look's data; views read it at runtime through the matching `UiTheme` field, not the const.
- **Parity checklist**: `docs/plans/native-client-parity.md`, every user-visible Tauri feature with its source file (paths on the `tauri-last` tag); the native client reaches parity when it is all ticked.
- **Passthrough variables**: the extra daemon-environment variables an agent plugin asks to reach its child beside the keep-list (`Agent::passthrough_vars`; Claude's are `ANTHROPIC_API_KEY` and `CLAUDE_CONFIG_DIR`).
- **Peer identity**: the tailnet peer's whois identity (node, user and stable id) attached to a connection that arrives on the tailnet listener.
- **Pending create**: the native client's ordered queue of request ids for tabs it asked to create and will make active on arrival; a refusing `Error` or a new `Welcome` removes ids from it.
- **Pending input**: what a hook-reported `AwaitingInput` is waiting on (a question, a permission or a plan approval), on the session record and snapshot as `pending_input` and in `meta.json` beside `status`, restored by a reattach only with `AwaitingInput`; see `docs/plans/hook-status.md`.
- **Petal**: the approved refreshed look for the native client (graphite ground, tulip-coral accent, status shapes), its settled design in `docs/native-client.md` ("Petal chrome", "Theme and fonts").
- **PF, AT, SV, RD, AR, OT**: idea ids in `docs/plans/off-orca.md` (files across machines, attention streams, session views, remote desktop, artifacts by account, absorbing ops-tower).
- **Plan snapshot**: `docs/plans/MAIN.md`, generated from the Linear team's issues, one line an item, by wave in working order; never edited by hand.
- **Poller**: the task that runs a waiting session's discovery passes, every 2 s for 5 minutes, every 10 s up to an hour, then every minute while the session runs (`crates/daemon/src/conversations/discovery.rs`).
- **Pop-out**: a second window of the native client showing a pane, a tab (a grid or a diff) or a session that has no pane; it is a window of the same process and connection, and closing it docks it back; see `docs/native-client.md` ("Windows and pop-outs").
- **Popped card**: the card a popped pane (in its grid slot) or an active popped tab leaves in the main window, with the label, Focus window and Dock back; it is not a terminal and holds no keyboard.
- **Pre-create**: an agent's conversation id obtained by running its CLI once before the session starts, the way some agents (not Claude) need; the daemon's runner is `crates/daemon/src/conversations/pre_create.rs`.
- **Preset**: a named launch recipe the daemon stores for a repo or workspace: the prompt sources it allows, its variables and its target tabs, launched through the preset wizard.
- **Preset job**: one preset launch the native client started, named by a client-made job id; the client's registry follows it through the daemon's progress messages and shows one toast for it.
- **Preset wizard**: the native client's modal that walks a preset's prompt source and variables before launching it; see `docs/native-client.md`.
- **Probe group**: the readings of one terminal path link, sent as fetches one at a time; a reading the host rejects before the transfer starts moves on to the next. See `docs/native-client.md` ("Fetched files").
- **Progress update**: an attention-feed item for a stop while work continues, or a summary of work in progress; shown in the feed, never pushed to the phone.
- **Prompt source**: where a preset launch takes its prompts from: a prompts file, a folder of `.md` files, inline text or GitHub issue ranges, limited to the sources the preset allows.
- **Prompt stash**: the TUI's one-slot draft store (ctrl+s), shared by every view.
- **Provider table**: a built-in, not user-editable list of providers. The Claude plugin's lists the environment variables each non-Anthropic provider sets, removes and locks (`crates/agents/claude/src/providers.rs`); the summarizer's lists each provider's endpoint, model and prices (`crates/daemon/src/summarizer/providers.rs`).
- **Provisional line**: a working refresh's summary line (DB.17): pushed to the front of a session's summary ring every 3 minutes while a Dashboard is watched, replaced by the next line, never written to `summary.json`, and dropped when the turn ends with no stop line on its way.
- **Quarantine**: an account whose refresh token was rejected (`invalid_grant`), skipped until it is logged in again.
- **Queue strip**: the Dashboard's By status band under the header while any session waits: one chip per wait, longest first, a click scrolling that session's row into view; see `docs/plans/dashboard.md`.
- **Re-ask**: the native client resending a queued in-place spawn, unchanged, when its checkout prompt's turn comes, so the daemon answers with current numbers instead of the stale prompt being shown.
- **Reached tests**: the tests a change's diff reaches through the test map, as one nextest filterset; `.\rt.ps1 test -Reached` runs them, and a central or unmapped file reaches the full suite instead.
- **Reading**: one way to take a terminal path link's path as a file under a repo or worktree of the pane's session; a link has one reading per candidate. See `docs/native-client.md` ("Fetched files").
- **Ready signal**: how much output an agent's TUI must have printed, and how long it must then stay quiet, before it counts as up (`Agent::ready_signal`, `ReadySignal`; Claude's is 1024 bytes then 1.5 s).
- **Ready update**: a newer release the native client has downloaded and checked against its `.sha256`, waiting in an `updates\<version>-<random>\` folder for the owner's `Restart to update`; the footer shows `Update {version} ready` while one is held (`docs/auto-update.md`).
- **Receipt**: the chat entry an answered prompt leaves behind: its question and the answer given.
- **Recovery**: respawning a session from its history entry: a Claude entry with `claude --resume <conversation id>`, as a Claude session or as a shell that types the command.
- **Remote mode**: the native client connected to a tailnet or SSH host (`Target::Tailnet`, `Target::Ssh`) instead of the local daemon; local-file actions are disabled with a reason, Stop and the daemon's files are hidden, and quitting only disconnects. See `docs/native-client.md`.
- **Remote status**: the daemon's `RemoteStatus` message, pushed at connect and broadcast on every change, whose `state` (`RemoteState`) says why remote access is off, down or serving (`crates/protocol/src/lib.rs`).
- **remote.json**: the config-dir file that turns the daemon's remote (tailnet) access on: `{enabled, port}`, absent until the owner turns it on (the daemon writes it when `ConfigureRemote` changes it), with the port defaulting to 8787; a missing, unreadable or zero-port file means off (`crates/daemon/src/remote_config.rs`).
- **Reserved arguments**: the flags an agent plugin builds itself (`Agent::reserved_args`; Claude reserves 16, such as `--settings`, `--resume` and `-p`), which a launch command's extra arguments may not repeat.
- **Ring (keyboard)**: a native dialog's ordered, wrapping list of stops that Tab and Shift-Tab walk; not the drawn focus ring, the outline around the focused control; see `docs/native-client.md` ("Keyboard reachability").
- **Routing variables**: the `ANTHROPIC_*` environment variables, and the `CLAUDE_*` ones that are not login or config, that point Claude Code at another endpoint, credential and model; built at each spawn, never stored. The summarizer's CLI call passes `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, which are login and config.
- **Rule trace**: `CompiledRules::explain`'s account of one screen reading: every rule with its region text and whether it matched or a guard vetoed it, the winner and the rejected rules; `rustling-tulipd screen <session id>` prints it beside the grid.
- **Running-work strip**: the row over the chat and summary composers listing a session's running subagents, background shells and next wakeup (RT-107).
- **Safe option**: the non-destructive button a confirm dialog opens on — Cancel, Keep running, Ignore, or a pane-only close; see `docs/native-client.md` ("Keyboard reachability").
- **Saved host**: a tailnet or SSH host the user added to the native client's host picker, stored in `native-ui.json`'s `saved_hosts` as `{name, port}` (tailnet) or `{kind: "ssh", name, remote_command}` and listed ahead of the scanned peers; `connection_picker::SavedHost`, `docs/native-client.md`.
- **Screen engine**: the daemon's per-session rendered grid (`crates/daemon/src/screen.rs`) and the evaluator that reads it with the agent's screen rules (`crates/daemon/src/screen/rules.rs`); a Claude session's status comes from it and from hooks, never from output volume or PTY silence. The chat mirror's dialog parsers (RT-51) and user rule files (RT-74) build on it; see `docs/plans/screen-status.md` and `docs/spikes/chat-mirror-tui.md`.
- **Screen rules**: an agent plugin's table of prioritised rules over regions of the rendered screen, each with guards and a verdict (working, awaiting input, idle or keep); `agent_api::ScreenRules`, evaluated by the daemon's `screen/rules.rs`. A table with any bad rule is refused whole (`ScreenRules::validate`), and that agent's screen then sets no status.
- **Screen-read count**: a running subagent or shell the strip shows from the screen alone, with no Stop record behind it yet.
- **Sealing**: the daemon's replacing of a secret row's literal value with a secret reference before the spawn config is captured, at spawn and, for older files, at startup; see `docs/env-secrets.md`.
- **Seam check**: `tools/check-agent-names.ps1`, which fails when `claude`, `anthropic` or `deepseek` appears in app code outside `crates/agents/claude/`, test code and the hits listed in `tools/agent-names-allow.txt`; it runs as a prek hook and first in `.\rt.ps1 clippy`.
- **Secret reference**: an environment-row value written `${secret:<id>}`, standing for a value the daemon saved in Windows Credential Manager; resolved at spawn, stored and echoed in place of the value.
- **Secret row**: an environment row whose key has a `KEY`, `TOKEN`, `SECRET` or `PASSWORD` segment or which the dialog's Secret toggle marks.
- **Select launched**: the button on a preset job's toast that tints that job's sessions in the sidebar (palette `LAUNCHED_TINT`) and shows the Sessions view; any session click or a `Welcome` clears the tint.
- **Session facts**: the per-session record the daemon sends only to connections watching a Dashboard (`DashboardFacts`): now line, agent tree, context, tokens, cost, activity counts, stuck reasons and issue.
- **Session history**: the daemon's record of ended sessions, one `history/<id>.json` per session under the config dir, kept 7 days; see `docs/architecture.md` (Session history and recovery) and `docs/native-client.md` (Session recovery).
- **Shared client core**: the planned Rust crate holding the tailnet connect and host profiles, used by both the native client's remote mode and the mobile app.
- **Spend cap**: the summarizer's daily cap in dollars (default $1.00): Working refreshes stop at half of it and Stop summaries become templates at the full cap, which is soft, so a call already running finishes; `docs/plans/dashboard.md` ("Privacy, cache, cap and fallbacks").
- **Spend ledger**: the summarizer's per-day spend record, `summarizer-spend.json` in the config dir (`{"date", "spent_usd"}` for the current local day); see "Where things live on disk" in `CLAUDE.md`.
- **Spike**: throwaway code that proves an approach works, kept outside the main build (`spikes/`) and deleted once its code is ported.
- **SSH target**: `Target::Ssh`, a host the native client reaches by running `ssh` with an `~/.ssh/config` alias or `user@host[:port]` and `rustling-tulipd bridge` (or the saved host's `remote_command`), speaking the protocol over the child's stdio; the SSH login is the owner, so there is no device token; `apps/native/src/ssh.rs`, `docs/native-client.md`, `docs/plans/ssh-remote.md`.
- **Staging folder**: `$INSTDIR\.staging`, where the installer extracts a setup's files before the staged `rustling-tulipd apply-update` swaps them into the install folder; the installer removes it afterwards (`docs/auto-update.md`).
- **Status glyph**: the shape-and-colour mark for a session's status in the native client (working arc, asking diamond, waiting ring, idle dot); see `docs/native-client.md`.
- **Status sequence (`status_seq`)**: the count of status changes on a `SessionSnapshot` (default 0), raised by one on each change (a reattach or orphan rebuild that changes the stored status included) and kept in the sidecar's `meta.json`; a client compares it to tell a newer snapshot from an older one.
- **Status-change log**: the planned `activity/<key>.jsonl` files in the config dir, one line per status change of a session or dispatch run, kept 2 hours and across daemon restarts, which the activity track draws from; see `docs/plans/dashboard.md`.
- **`status_since`**: the daemon's stamp of a session's last status change, on its snapshot and kept across a reattach; Needs You measures waits from it.
- **Sticky toast**: a toast with no timeout that stays until its × is clicked or something replaces it; a running preset job's toast and a failed launch's toast are sticky.
- **Stop (keyboard)**: a control Tab and Shift-Tab land on in a native dialog; see `docs/native-client.md` ("Keyboard reachability").
- **Stretch**: a run of chat entries the summary view shows as one line, a turn or a window of activity, keeping the range of entry ids it covers.
- **Subagent tree**: each session's subagents and background shells, read from the transcripts by the Claude plugin (`crates/agents/claude/src/subagents.rs`, `agent_api::SubagentNode`) for the Dashboard; see `docs/plans/dashboard.md` ("The transcript activity reader").
- **Subplan**: a `docs/plans/*.md` design file holding the design and rulings for open items, linked from their issues and deleted once nothing open links it and its durable text is promoted into `docs/`.
- **Summarizer**: the daemon service (`crates/daemon/src/summarizer/`) that turns a session's turn-end reports into a kind, a headline and a one-line summary, through Claude Haiku on the CLI or DeepSeek. Its settings, per-session queue, cache and spend ledger are in place, and finished-turn summaries run into each session's summary ring. See `docs/plans/dashboard.md`.
- **Summarizer folder**: the empty machine-local folder (`<data_local>/summarizer`) the summarizer's CLI call runs in; see "Where things live on disk" in `CLAUDE.md`.
- **Summary ring**: a session's last summary lines (at most 12, or 2 hours), newest first, kept in `sessions/<id>/summary.json`; the Dashboard's drawer shows it.
- **Summary view**: the planned third view of a session beside terminal and chat, one summarizer line per stretch of work (RT-111).
- **Suspend rig**: a driver that freezes only the process under test, for longer than the bound under test, then resumes it, so a starvation-only failure goes red without loading the machine; one long freeze reproduces while repeated short ones do not, since the process wins a run window.
- **Swatch line**: one row of colour swatches in the native client's Appearance views that is a single keyboard stop; Left and Right move along it.
- **Tailnet**: the user's Tailscale network; the daemon's tailnet listener serves TLS on the host's tailnet addresses (on the port `remote.json` configures, default 8787) with a `tailscale cert` certificate, and `crates/tailnet` wraps the `tailscale` CLI; see `docs/plans/remote-access.md`.
- **Target**: what the native client's connection dials: `Local` (the loopback daemon, ensured and reached with the `daemon.json` token), `Tailnet(host)` (a MagicDNS name and port over TLS with the device token) or `Ssh(host)` (an SSH target); `apps/native/src/target.rs`, `docs/native-client.md`.
- **tauri branch**: the branch holding the earlier, retired Tauri desktop app, cut from the `tauri-last` tag and kept as history. Nothing is fixed or built there, and daemon fixes are not cherry-picked to it.
- **Template line**: the free fallback summary line: no kind, the headline `Turn finished`, and the first non-blank line of the report.
- **Terminal title**: the title a session's program sets with an OSC sequence (`SessionSnapshot.terminal_title`); the client names a session by it after stripping the agent's title glyph, falling back to the session label.
- **Test connection**: the host picker's SSH add-form step that runs `ssh` with the typed host and remote command, sends a `Hello` with an empty token and shows `Connected: <short name>` or the failure (ssh's stderr, the daemon's refusal or a 20 s timeout), saving nothing; `docs/native-client.md`.
- **Test map**: `tools/test-map.json`, which maps source paths to the nextest filterset that tests them (`rows`), and lists the `central` files that run the full suite and the paths to `ignore`; `.\rt.ps1 test -Validate` checks it.
- **Test mode**: whether each landed change runs the full suite (`stable`) or only the fast checks and its reached tests (`unstable`, this repo's mode until the stable milestone, Wave 3's cutover); the full suite still runs before a push and before the release step.
- **test-release**: the Cargo profile tests build with (`cargo nextest run --cargo-profile test-release`; nextest's own `--profile` is not the build profile): release optimisation without LTO, with 16 codegen units and incremental builds, so an edit rebuilds in seconds.
- **Title rules**: an agent descriptor's `title` (`protocol::TitleRules`): the leading glyphs (character ranges) its terminal title carries and the placeholder title it shows when it names nothing, which the client strips from a session's name (`session_title` in `apps/native/src/sidebar.rs`).
- **Toast activator**: the COM class (`toast_activation::ACTIVATOR_CLSID`, registered under `HKCU\Software\Classes\CLSID`) Windows starts or calls when a toast button is pressed from Action Center, so the press reaches the native client even when it is closed.
- **Tracer-log import**: the startup pass that rebuilds history entries from `logs/tracer-<id>.log` files for sessions that ended before history existed; a shell-hosted agent comes back as that agent from its agent note, an unregistered agent or unreadable note as a plain shell.
- **Tracking issue**: the issue `Merge feat/<name> (#N)`, last in a feature marker's project, that stands for its feature PR until the PR merges.
- **Transcript tailer**: the daemon's reader of a growing JSONL file from a byte offset (`crates/daemon/src/transcript_tail.rs`, planned), shared by the chat mirror, the file-path index and the subagent watcher.
- **Trigger**: what asks the summarizer for a line: `stop`, a finished turn, or `working`, a refresh of a working session.
- **Typed shell form**: the way an interactive agent session starts, the daemon typing `<cd> && <command line>` into the user's shell (agent in shell); with a launch command the program word is typed bare, the plugin's arguments quoted and the extra arguments verbatim.
- **UI text size**: the interface text offset, a whole number of pixels from −2 to +4 (`native-ui.json`'s `ui_text_size`, 0 by default), added to the base size of the chrome text and its row heights through `fonts::ui_px` (`apps/native/src/fonts.rs`); terminal, viewer and diff-gutter text keep the terminal font size.
- **UI theme**: the `UiTheme` the native client's window draws its colours with, one field per palette token; views read the active one with `theme(cx)` and `RootView::set_theme` swaps it (`apps/native/src/ui_theme.rs`), and the default is Nord (`theme_store::DEFAULT_THEME`).

  The boot picks it from `native-ui.json`'s `theme` through `ThemeStore` (`theme_store.rs`; user themes are `<config dir>/themes/<slug>.json`); its `terminal` part is the terminal's palette (`TerminalPalette`, `theme.rs`).
- **Unexpected end**: a session whose tracer was lost (killed or crashed) rather than one that exited or was closed; these are pre-ticked for recovery.
- **Unseen turn**: an agent session whose turn ended while this client wasn't showing it focused; it shows the waiting ring until focused.
- **Unsummarized reports**: a session's pending `Stop` messages (at most 12, each capped at 8 KiB), waiting for the summarizer's next call.
- **Untouched (picked) layout**: a tab whose layout type was last chosen by a pick (Rearrange or the first-connect chooser) and has not been hand-resized (a divider drag) or hand-arranged since; the daemon rebuilds that type as panes come and go, and the per-client flag is `TabEntry.picked_layout`; see `docs/native-client.md` ("Tab layouts").

  A pick of the automatic grid also remembers the pane area's aspect and follows it when the window is resized (that re-fit keeps the tab's empty panes, where a fixed pick just keeps its count).
- **Waiter**: a `DiscoverAfterLaunch` session registered at spawn, with its baseline and launch time, until discovery claims a conversation for it or its final look runs.
- **Waiting devices**: the owner's list of tailnet devices whose pairing request awaits approval, in Settings → Remote access; its model is `apps/native/src/pairing.rs` and its views are in `apps/native/src/pairing_view.rs`.
- **Warm-up gate**: auto-switch making no decision until every usable account has been polled since it started.
- **Wave**: a release-sized bundle of plan items that share owning files, so one implementer reads those files once and one review covers the bundle; each wave is a Linear project whose content names its files, review and verification.
- **Worker**: a subagent or rustling-tulip session running one brief in one worktree.
- **Working refresh**: the summary refresh of a working session while the Dashboard is watched, made from the transcript's last assistant text.
- **Worktree seeding**: preparing a new worktree before its agent starts by copying the ignored files a repo lists, linking shared folders and running its setup script; a planned idea (RT-71, `docs/plans/borrowed-ideas.md`).
