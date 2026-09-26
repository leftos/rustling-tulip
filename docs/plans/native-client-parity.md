# Native client: feature parity checklist

Every user-visible feature of the Tauri client, traced to the code that implements it, so the native client can reach parity area by area. Frontend paths are relative to `apps/tauri-app/src/`; host paths start with `src-tauri/src/`. **(hard)** marks items with no ready-made native equivalent. Inventory taken 2026-09-23 (148 items); parent plan: [native-client.md](./native-client.md).

## Riskiest for the port

1. **Terminal emulation + GPU rendering** (`components/Terminal.tsx`): `alacritty_terminal` + the GPUI grid must match xterm on bracketed paste, cursor show/hide/shape, alt screen, mouse reporting, cell width, glyph fallback.
2. **Link detection with wrapped-path stitching and Ctrl-hover** (`utils/terminalLinks.ts`, `open_terminal_path`): needs row wrap flags, per-link hover and decorations on the new buffer model.
3. **Shell-integration gutter dots** (`components/shellIntegration.ts`): OSC 133/633 handling plus line markers that survive scroll, reflow and scrollback trim, with clickable overlays.
4. **Monaco diff editor** (`components/DiffPane.tsx`): a native side-by-side diff needs diffing, syntax highlighting, synced scroll and hunk navigation.
5. **Drag and drop + multi-window state** (`GridRenderer.tsx`, `TabBar.tsx`, `Sidebar.tsx`, `PaneWindow.tsx`, `utils/poppedPanes.ts`): shared drag types with edge detection; pop-outs coordinate via localStorage and each has its own daemon connection. In GPUI this becomes one process with shared state and several windows.

Close behind: the paste and key quirks (native clipboard read, Shift+Enter bytes per agent, Ctrl+C copy-vs-interrupt). They are small, but users notice immediately.

## Connection & daemon lifecycle

- [x] Start or reuse the local daemon: health probe, protocol check, stale-binary detection, cached binary copy, graceful `/shutdown` then force kill, orphan reaping, 30s handshake wait, spawn lock — `src-tauri/src/daemon_supervisor.rs`, `lib.rs` `ensure_daemon_started`
- [x] WS client: `Hello` with protocol versions, token, `client_id`, hostname; handles `welcome` / `auth_failed` — `api.ts` `connectDaemon`
- [x] Per-install client identity (persisted `client-id` file + hostname) — `lib.rs` `get_client_identity`
- [x] Auto-reconnect with backoff (0.5s to 10s), including after a failed handshake; suppressed after the user stops the daemon — `App.tsx`
- [x] Standby-resume watchdog: a 30s tick arriving >90s late probes liveness with `list_repos` (3s) and forces a reconnect if the socket is half-open — `App.tsx`
- [x] Full-screen "Starting/Connecting to daemon…" overlay with spinner and "Restart daemon", until the first connect — `App.tsx` `ConnectingOverlay`
- [x] Footer status pill: state dot, "N sessions", ":port", reason tooltip — `components/DaemonFooter.tsx`
- [ ] Footer pill remote-host chip (with the connection picker, Phase 6) — `components/DaemonFooter.tsx`
- [x] Footer troubleshooting flyout: state, port, pid, protocol, session count, reason, copy handshake path; Esc / outside click closes — `DaemonFooter.tsx`
- [x] Open daemon.log / app.log in the default app; reveal the config dir (native: daemon.log and native.log) — `DaemonFooter.tsx`, `lib.rs` `daemon_paths`
- [x] Restart daemon (graceful `shutdown` then respawn; forced fresh connect in auth-failed/stopped) — `App.tsx` `onRestartDaemon`
- [ ] "Reconnect" in place of Restart on a remote connection (Phase 6) — `App.tsx` `onRestartDaemon`
- [x] Stop daemon with two-click confirm (kill pid, remove handshake, no respawn) — `DaemonFooter.tsx`, `lib.rs` `stop_daemon`
- [x] Main-window close intercepted; quits silently when no sessions are active — `App.tsx` `onCloseRequested`
- [x] Exit dialog: keep running (default), stop and keep worktrees, stop and remove worktrees (branch fate per session, "Session n of m"), abandon and quit, orphan note, force quit after 5s stuck — `ExitConfirmDialog.tsx`, `utils/exitWorktreeQueue.ts`
- [x] Wait for `shutdown_ack` or WS close, then exit from the host side — `App.tsx`, `lib.rs` `quit_app`
- [x] app.log rotation on boot; frontend logging through `log_message` — `lib.rs`, `utils/logger.ts` (native: tracing writes `logs/native.log`, rotated to `native.log.old` on boot)
- [ ] First-connect layout chooser (cannot be dismissed): start empty / open all active sessions (grid, side-by-side or stacked, max per tab) / adopt previous / copy another client's layout — `LayoutChooser.tsx`
- [ ] Daemon `error` becomes a toast and cancels pending spawn routing; unknown message types logged — `App.tsx` `handleMessage` (native: the toast landed in P1.7c; it cancels only the spawn whose request_id it carries)
- [ ] Main window size and position persisted — `lib.rs` (window-state plugin)

## Sidebar / repos / workspaces

- [ ] Activity bar (Sessions / Source control) with a change badge (caps at 99+); clicking the active item collapses the sidebar; persisted — `ActivityBar.tsx`
- [x] Resizable, collapsible sidebar with persisted width — `ResizableSplit.tsx`
- [ ] Header: brand, Settings, Repos/Tabs view toggle (also saved as the default) — `Sidebar.tsx`
- [ ] Toolbar: + Session (disabled with "needs repo"), + Shell, Shell…, + Repo (picker remembers the last dir), + Workspace (needs 2 repos), "Resume all (N)" — `Sidebar.tsx` (native: + Session landed in P1.7c, + Shell and Shell… in P1.7d, in a toolbar row under the header that wraps)
- [ ] Repos view: workspace, repo, SH and DIR containers plus a "Detached" bucket with a banner — `Sidebar.tsx` `buildContainers` (native: containers and Detached bucket done in P1.4; the Detached banner is still missing)
- [x] Plain-shell sessions regroup under the container matching their live cwd — `Sidebar.tsx` `findContainerForCwd`
- [ ] Tabs view: one container per tab plus an "Unbound" bucket with a banner — `Sidebar.tsx` `buildTabContainers`
- [ ] Container row: collapse chip (Enter/Space), count, "!" roll-up, kind tag, last-launch summary, ▶ launch-last; double-click launches last in the current tab — `Sidebar.tsx` `ContainerNode`
- [ ] Remove repo/workspace: inline two-click; with live sessions a dialog (Cancel / Remove anyway / Stop and remove) — `Sidebar.tsx`, `RepoRemoveDialog.tsx`
- [ ] Detached container "stop all" with two-click confirm — `Sidebar.tsx`
- [ ] DIR/SH containers: "Add repo"; "Add workspace" when a `.code-workspace` is found — `Sidebar.tsx`
- [ ] Drag-reorder containers, tab containers (shared with the TabBar) and leaves, saved on the daemon — `Sidebar.tsx`
- [ ] Container context menu: spawn (here / in tab), Launch last again ▸ current / new / named tab / edit first, Appearance…, Launch preset (loading / failed / none), open in Explorer, open in VS Code (repo, linked or multi-root), copy path, Remove — `Sidebar.tsx` `ContainerContextMenu`
- [ ] Session leaf: status dot (pulses while working, hollow while spawning), label and tooltip, runtime tag, accent stripe, trusted marker, "!", orphan / abandoned / inactive tags, Resume/Dismiss, tab pill (`T:name`, `T:×N`, unbound button) — `Sidebar.tsx` `SessionLeaf`, `TabPill`
- [ ] Click a leaf to jump to its tab and pane and clear attention; double-click an unbound leaf to add it to the active tab — `Sidebar.tsx`, `App.tsx` `onSelectSession`
- [ ] Drag a leaf onto a pane or a tab pill — `Sidebar.tsx`
- [ ] Workspace creator: from repos (name + ≥2 members) or from a VS Code workspace file (parse, show registered / will register) — `WorkspaceCreator.tsx`
- [ ] Daemon-pushed "VS Code workspace detected" prompt: Not now / Create / Create & watch — `VscodeSuggestionToast.tsx`
- [ ] Empty states: no repos (different wording on remote); main area shows Select a tab / Spawn a session / Open shell + Add repo — `Sidebar.tsx`, `App.tsx` `EmptyState` (native: Spawn a session and Open shell landed in P1.7d; Add repo waits for the repo picker)

## Sessions

- [ ] Session context menu: rename inline (blank restores the default), Duplicate ▸ new tab (Shift = prefilled dialog) or an existing tab, Move to ▸, Add to current / new tab, Pop out, Appearance…, Accent ▸ (presets / recent / custom / inherit), Reveal worktree — `SessionContextMenu.tsx`, `MoveToSubmenu.tsx`, `MenuSubmenu.tsx`
- [x] Actions by state: running → Stop (delete or keep worktree); stopped → Restart / park / remove (± worktree); inactive → Resume / remove (± worktree) — `SessionContextMenu.tsx`
- [x] Stopping a session with no pane parks it or discards it — `SessionContextMenu.tsx`
- [ ] Pane header: status dot, label, runtime chip, trusted chip, "· headless", one repo:branch chip per member (path in tooltip), Pop out, two-step Stop or "exit code N" — `SessionPane.tsx`
- [x] Stopped-pane overlay: Restart in place, New session… into this pane, remove pane keep worktree, remove pane (± worktree) — `SessionPane.tsx`
- [ ] Abandoned overlay (shows the last prompt) with Resume / Dismiss; orphan banner — `SessionPane.tsx`
- [ ] Headless view: status, tokens in/out, cost, recent-actions log (last 200, "Show all") — `SessionPane.tsx` `HeadlessView`
- [ ] Display label order: user label → shell cwd name → terminal title (skipping bare shell names) → daemon label → runtime — `utils/sessionLabel.ts`
- [ ] Sessions without a worktree that exit on their own are discarded automatically — `App.tsx`
- [x] Delete-worktree confirm (the only path to deleting one): per-branch fate from the daemon, delete all / keep vs delete (commits lost) / worktree only, 10s fallback, safe option focused — `DeleteWorktreeDialog.tsx`, `utils/branchFate.ts`
- [ ] Worktree cleanup failed: path, reason, locking processes (name, pid, cmdline) as checkboxes, open folder, "Kill N & retry" / Retry / Ignore — `WorktreeCleanupFailedDialog.tsx`
- [x] Blocking "action failed" modal (title, detail, hint) — `ActionFailedModal.tsx`

## Spawn dialog & launch flows

- [ ] Entry points: toolbar, Ctrl+N, container menu (target locked), empty pane (preselected), tab container (tab fixed), duplicate (prefilled), worktree manager (worktree pinned) — `SpawnDialog.tsx` (native: toolbar and Ctrl+N / Ctrl+Shift+N landed in P1.7c)
- [ ] Target picker ([REPO]/[WS]) or fixed label; "no repos" state with + Add repo — `SpawnDialog.tsx` (native: the picker landed in P1.7c; no "no repos" state, since native has no Add repo yet)
- [x] Runtime radio: claude / codex / cursor / plain shell; defaults to the target's last spawn unless the user changed it — `SpawnDialog.tsx`
- [x] "Open in": current tab / new tab / each other tab — `SpawnDialog.tsx`
- [ ] Mode Interactive / Headless (prompt textarea; no headless for cursor) — `SpawnDialog.tsx`
- [x] Trusted launch checkbox (per-runtime skip-permissions / yolo flag) with a warning banner — `SpawnDialog.tsx`
- [ ] Advanced: model, Claude approval mode, Codex sandbox, Cursor plan mode + sandbox, env vars (invalid-key and duplicate warnings) — `SpawnDialog.tsx`
- [x] Single-repo: create worktree (saved per repo), new / use existing, picker (in use / stopped / stale, size, age), confirm before sharing with a live session — `SpawnDialog.tsx` `SingleForm`
- [ ] Branch combobox: lists all branches, filters as you type, marks current, "Create branch" row, arrow/Enter, Esc closes the list only — `BranchCombobox.tsx`
- [x] Suggested branch name, "Random", "picking a name…", cached between opens — `SpawnDialog.tsx` `useBranchField`, `utils/branchSuggestion.ts`
- [ ] Base branch defaults to `origin/<default>`; notes a failed background fetch; debounced preview for N commits behind and existing worktree/branch (reuse or recreate) — `SpawnDialog.tsx` (native: the default and the fetch note landed in P1.7c; the preview is Phase 4)
- [ ] Workspace form: one branch for all members, create worktrees, new / existing group ("N bound, M to be created"), base, preview table — `SpawnDialog.tsx` `WorkspaceForm` (native: all but the preview table landed in P1.7c)
- [x] Double-submit guard; Esc closes, backdrop click doesn't — `SpawnDialog.tsx`
- [x] Dirty in-place checkout prompts Carry changes / Stash & switch, then resends — `CheckoutConfirmModal.tsx`
- [x] "Spawning session…" toast; placement (new tab / this pane / smart) and terminal focus — `App.tsx`, `utils/autofocus.ts`
- [ ] Launch last again: replays the config; worktree launches wait for a fresh branch name (toast on timeout); trusted configs open the full dialog with a warning — `App.tsx` `onLaunchLast`
- [x] Standalone shell: quick default dir, or a dialog with Browse and "Use as quick shell default" — `StandaloneShellDialog.tsx`
- [ ] Preset wizard: source (file / folder / inline / GitHub issue ranges) → variables (toggle / file / folder / text, required fields) → preview (grouped by tab, max panes per tab, script commands) → launching (progress, counts, Cancel, Select launched, Stop all) — `PresetLaunchDialog.tsx`, `utils/parsePrompts.ts`, `utils/parseIssueSpec.ts`
- [ ] Sticky preset progress and failure toasts, one per job; the first tab created becomes active — `App.tsx`

## Tabs & panes / layout

- [ ] Tab strip: pills with Δ for diff tabs, busy/total badge, name; + New tab — `TabBar.tsx` (native: all but the busy/total badge landed in P1.5)
- [ ] Click activates; Ctrl/Cmd-click toggles selection; Shift-click selects a range — `TabBar.tsx` (native: click-to-activate landed in P1.5)
- [x] Close with × (two-click when the tab has sessions or ≥2 panes; Esc / outside click resets); middle-click closes — `TabBar.tsx`
- [x] Double-click to rename inline — `TabBar.tsx`
- [ ] Drag to reorder; a pane dragged over a pill activates that tab; dropping on a pill places the pane automatically — `TabBar.tsx`
- [ ] Tab menu: Rename, Pop out, Rearrange ▸ grid (auto / N×M) / side by side / stacked, Move panes to new tab… (≥3 panes), font +/−/reset, Close, Close others, Merge selected (horizontal / vertical) — `TabBar.tsx`
- [ ] Move panes dialog: pick panes, layout, grid shape, name — `MovePanesDialog.tsx`
- [ ] Undo shelf for closed tab / closed pane / move / swap (8s, at most 3) — `UndoShelf.tsx`
- [x] Split tree with draggable dividers (5–95%), ratio saved on release — `GridRenderer.tsx` `SplitRenderer`
- [x] Focused pane remembered per tab; a new split gets focus — `utils/grid.ts`
- [ ] Split right/down (Shift = left/up), move to new tab, close pane from the header; empty panes get floating buttons — `SessionPane.tsx`, `GridRenderer.tsx` (native: split and close landed in P1.5; move to new tab and the empty-pane buttons are open)
- [ ] **(hard)** Pane drag and drop from the header or ⠿ handle: edge overlay for splits, centre swap, outer band splits at the top level, across tabs — `GridRenderer.tsx` `computeEdge`
- [ ] Closing a pane with a session: close pane only / discard session keep worktree / delete worktree — `PaneCloseDialog.tsx`
- [ ] Empty pane placeholder and its context menu (Move to ▸, Close) — `EmptyPane.tsx`, `GridRenderer.tsx` `PaneContextMenu`
- [x] Smart placement: next to the same repo → an empty pane → split the largest pane along its longer side — `App.tsx` `paneTargetForSession`
- [ ] "Import remote sessions" modal when the remote session count changes — `ImportArrangementModal.tsx`
- [x] Active tab remembered across reloads — `App.tsx`
- [ ] Window title "(M/N) Tab — rustling-tulip", debounced 350ms — `utils/windowTitle.ts`

## Terminal

- [ ] **(hard)** Rendering parity with xterm 6 + WebGL (Fit, WebFonts, Clipboard addons; DOM fallback on GPU context loss) — `components/Terminal.tsx`
- [x] **(hard)** Scrollback first with live output buffered and drained after; retries at 2s/4s with an in-place status line; truncated / failed banners; 5000-line scrollback — `Terminal.tsx`, `api.ts` `loadScrollback` (native: all but the 5000-line cap landed in P1.6)
- [x] UTF-8 split across chunks; `detach` on unmount — `Terminal.tsx`
- [x] Refit + `resize` on container resize, font change, scrollback drawn — `Terminal.tsx`
- [x] Input dropped once the session is stopped or errored — `Terminal.tsx`
- [x] Cursor: bar for agents, block for shells, no blink; programs can hide it or change its shape — `Terminal.tsx`
- [x] **(hard)** Shift+Enter newline: `\` + CR for claude and shells, `\n` for codex/cursor — `Terminal.tsx`
- [x] Ctrl/Cmd+C copies with a selection, else sends ^C; Ctrl+Shift+C always copies — `Terminal.tsx`
- [x] **(hard)** Paste via the native clipboard read (WebView2 can truncate), bracketed when the program asks, every paste logged — `Terminal.tsx`, `lib.rs` `read_clipboard_text` (native: GPUI clipboard, bracketed and sanitised, in P1.6; paste logging still missing)
- [x] Copy on select (setting, on by default) — `Terminal.tsx`
- [x] **(hard)** OSC 52: program clipboard writes reach the system clipboard and show the "copied" chip; reads answered empty — `components/clipboardProvider.ts`
- [x] **(hard)** Links: URLs and paths (absolute, UNC, relative, `./`, `../`) with `:line:col`; trims trailing punctuation; stitches wrapped rows including TUI box borders (up to 4 joins within ±64 rows); shorter fallbacks if the path doesn't exist — `utils/terminalLinks.ts`
- [x] **(hard)** Links underline only while Ctrl/Cmd is held; Ctrl/Cmd-click opens URLs in the browser, paths via cwd/worktree resolution in VS Code `-g` at the line or the default app / file manager; disabled on remote with a toast — `Terminal.tsx`, `lib.rs` `open_terminal_path`
- [ ] **(hard)** Shell integration (plain shells): OSC 133 A/B/C/D + OSC 633 E; gutter dot per command (ok / fail / unknown, exit code and duration tooltip), 1000-command cap — `components/shellIntegration.ts`
- [ ] Gutter-dot menu: exit and duration, copy command / output / both, re-run (types the command without Enter) — `ShellCommandMenu.tsx`
- [x] Theme: fixed palette, ANSI colours contrast-adjusted against the background, white caret, blue selection — `utils/terminalTheme.ts`
- [ ] **(hard)** Fonts: Geist Mono preloaded; Fira Code / JetBrains Mono / Cascadia Code bundled; system fonts listed; bold toggle; size 8–32; applied live — `Terminal.tsx`, `utils/bundledFonts.ts` (native: P2.8 bundled the four families with italics (none for Fira Code) and applies a font model live; choosing a family, the bold toggle and the system list come with the P2.10 editor)
- [ ] Font size per app / repo / session (Ctrl+= / − / 0) / tab (Ctrl+Shift+= / −, tab menu) — `utils/fontSize.ts`
- [ ] Focus goes to the terminal after spawn and when a pop-out opens — `utils/autofocus.ts`

The frontend has no search addon, bell handling or title parsing; `terminal_title`, `current_cwd` and `program_name` come from the daemon in each session snapshot.

## Source control & diff

- [ ] Two modes: per-member sections for the focused session, or a single repo (follows the active pane or a saved dropdown choice) — `source-control/SourceControlSidebar.tsx`
- [ ] Refresh; collapsible sections (dirty ones open, clean closed; saved) — `SourceControlSidebar.tsx`
- [ ] Staged / Changes folder trees with M/A/D/R/U status; per-file hover buttons (unstage; discard, stage); Unstage all / Discard all / Stage all — `ChangesTree.tsx`, `utils/changesTree.ts`
- [ ] Commit box once something is staged; Ctrl+Enter; "Committing…"; dismissable error — `SourceControlSidebar.tsx`
- [ ] File menu: open (staged) changes, stage / unstage, discard with a confirm listing the paths — `DiscardConfirmDialog.tsx`
- [ ] Stashes: collapsible, stash with optional message, pop / apply / drop, live updates — `StashesSection.tsx`
- [ ] History: 50 at a time with "load more", hover card (sha, author, date); selecting a commit shows header, body and file list — `SourceControlSidebar.tsx` `HistoryView`, `DiffView`
- [ ] Open in forge (GitHub / GitLab / Bitbucket) — `api.ts` `getRemoteUrl`
- [ ] Click a file to open or focus its diff tab — `api.ts` `openDiffTab`
- [ ] **(hard)** Side-by-side read-only diff: path, "worktree vs index" / "vs HEAD", whitespace toggle (saved), change count, first / previous / next / last (wraps), loading and error states — `components/DiffPane.tsx` (Monaco `createDiffEditor`; language from the daemon; no language services)
- [ ] Resizable changes / history split — `ResizableSplit.tsx`

## Worktrees

- [ ] Worktrees root setting: path, Browse, Save, Reset to default, override indicator — `SettingsModal.tsx` `WorktreesPanel`
- [ ] Manage worktrees modal: group status (Active / Detached / Stale / Unknown), path, branch, size, age, session and members; Refresh; Delete (size in confirm); Delete all stale; Launch session here (confirm if one is running) — `WorktreesManagerModal.tsx`

## Settings & appearance

- [ ] Settings tabs: General / Notifications / Spawn defaults / Worktrees / Remote access / Appearance / App Title; changes save immediately — `SettingsModal.tsx`, `utils/settings.ts`
- [ ] General: keep the machine awake (with status), default sidebar view, copy on select — `SettingsModal.tsx`
- [ ] Spawn defaults: trusted default, Claude approval mode, Codex sandbox — `SettingsModal.tsx`
- [ ] App title: busy count and product suffix toggles, live preview — `SettingsModal.tsx`
- [ ] Appearance editor at app / repo-workspace / session level: accent colour, shell background (presets, 12 recent, custom), font family, size, bold, each showing its resolved value and source — `AppearanceEditor.tsx`, `utils/appearance.ts`
- [ ] Session accent drives the sidebar stripe, pane frame and focus colour — `SessionPane.tsx`, `utils/sessionColor.ts`

## Notifications & attention

- [ ] OS notifications for awaiting input / stopped / error, each toggleable; body is the session label; permission requested at startup — `App.tsx`
- [ ] Notifications settings: permission badge, "Request permission" — `SettingsModal.tsx`
- [ ] Attention: leaf highlight and "!", container roll-up; cleared when the user selects the session or it calms down — `App.tsx`, `Sidebar.tsx`
- [ ] Toasts: error / warning / info, 8s auto-dismiss, optional sticky, same-key toasts update in place — `ErrorToast.tsx` (native: error and info toasts with 8s auto-dismiss and × landed in P1.7c; no sticky or same-key update)
- [ ] Toasts for failed git writes and for actions unavailable on remote — `App.tsx`
- [ ] "✓ copied" chip after a confirmed clipboard write — `CopyPulse.tsx`, `utils/clipboard.ts`
- [ ] Preset-launched sessions highlighted in the sidebar — `App.tsx`

## Pop-out windows

- [ ] **(hard)** Pane pop-out: toolbar (status, label, source tab, members, runtime, trusted), Stop, Dock back; the main grid keeps a "popped out" card (Focus window / Dock back); closes itself when the pane or session goes — `PaneWindow.tsx`, `GridRenderer.tsx` `PoppedOutPaneCard`, `utils/poppedPanes.ts`, `lib.rs` `open_pane_window`
- [ ] Tab pop-out: full grid (split, drag, close) or a diff tab, Close window; spawning disabled with a hint; closes when the tab is removed — `TabWindow.tsx`
- [ ] Session pop-out (session with no pane): Stop, Close window; stays open after the session stops — `SessionWindow.tsx`
- [ ] **(hard)** Opening an existing pop-out focuses it; title is the session or tab name; 1100×720 (min 700×400); "not found" state — `lib.rs`
- [ ] Pop-outs have no app shortcuts and no Pop out button of their own — `App.tsx`, `SessionPane.tsx`

## Remote / LAN

- [ ] Connection picker: this machine, saved hosts (select / forget), LAN discovery (2.5s mDNS) then pairing code, or paste a connection code — `ConnectionPicker.tsx`
- [ ] Host LAN panel: enable, port, advertised address, cert fingerprint, connection code with Copy — `SettingsModal.tsx` `LanPanel`
- [ ] Pair a device: one-time code with countdown and Cancel, and the pairing outcome — `SettingsModal.tsx` `PairDevicePanel`
- [ ] Start daemon on login (HKCU Run key / LaunchAgent) — `src-tauri/src/autostart.rs`
- [ ] Pinned-TLS tunnel (SHA-256, trust on first use), 8s probe, local plaintext bridge, pairing over the same pinned TLS, hosts saved in `remote-profiles.json` — `src-tauri/src/remote.rs`
- [ ] Remote mode disables local-file actions (add repo, reveal, VS Code, pickers become typed paths, terminal path links) — `utils/remoteMode.ts`
- [ ] Chosen connection saved; a missing saved host falls back to local — `App.tsx`

## Misc & shortcuts

- [ ] App shortcuts, ignored in inputs, the terminal, modals and pop-outs: Ctrl+B sidebar, Ctrl+T new tab, Ctrl+N spawn, Ctrl+, settings, Ctrl+(Shift+)Tab cycle tabs, Ctrl+1–9 jump to tab, Ctrl+Shift+G auto-grid, Ctrl+= / − / 0 session font, Ctrl+Shift+= / − tab font — `utils/a11y.ts`, `App.tsx`
- [ ] Other keys: Ctrl+Enter commits; Enter/Esc in rename fields; Esc closes menus and modals; arrows in the branch combobox — various
- [ ] Default right-click menu suppressed everywhere, Monaco included — `main.tsx`
- [ ] Menus stay inside the viewport; modals focus the safe option and return focus on close — `utils/a11y.ts`
- [ ] File pickers remember the last folder per purpose — `api.ts`

## Host capabilities to replace

Today these live in Tauri commands and plugins; natively they become plain Rust calls in the client process.

- Daemon supervision, handshake, paths, client identity, stop: `ensure_daemon_started`, `daemon_paths`, `get_client_identity`, `stop_daemon` (the logic lives in `crates/daemon-client`; the Tauri commands are thin wrappers).
- Native pickers: `pick_directory`, `pick_file`, dialog plugin (e.g. `rfd`).
- Windows: `open_session_window`, `open_pane_window`, `open_tab_window`, window-state persistence, close interception, `setTitle`.
- OS handoff: `reveal_in_explorer`, `open_url` (http(s) only), `open_terminal_path`, `open_folders_in_vscode`, opener / shell plugins.
- Clipboard: `read_clipboard_text` (arboard) plus writes.
- Notifications: permission and send (notification plugin).
- Logging and quit: `log_message`, `quit_app`.
- Remote: `connect_remote`, `disconnect_remote`, `decode_connection_code`, remote profiles, `discover_lan_hosts`, `pair_with_host` (`src-tauri/src/remote.rs`, reusable as a library).
- Autostart: `get_autostart`, `set_autostart`.
- Tauri's `tray-icon` feature is enabled, but no tray code exists.
