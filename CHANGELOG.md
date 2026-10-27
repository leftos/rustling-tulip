# Changelog

What changed in each release of rustling-tulip, newest first. Installers are on the [releases page](https://github.com/leftos/rustling-tulip/releases).

## Unreleased

### Added

- The Needs You panel says what a waiting session asks: its question, the tool it wants to run, or the plan to approve, cut short when it runs long.
- On a remote host the preset wizard takes typed paths on that host for its prompt file, prompt folder and path variables.

### Fixed

- Before connecting, pairing, replacing or stopping the daemon, the app checks that the program on its port is really the daemon, so your key never reaches another program.
- A connection the daemon refuses always shows why, instead of sometimes dropping with no reason.
- A session brought back from Recover keeps its name, colours and place in the sidebar.
- Recovering a session in a shell never lets a `$`, `%`, `!` or backtick in your config folder's path run as shell code.
- A Worktrees folder the daemon can't create no longer leaves Settings showing it as saved after another window changes it.
- A text field you aren't typing in no longer shows its text selected, in Settings, the Spawn dialog and everywhere else.
- In Settings, when saving or applying makes the focused button go away, the keyboard moves to the next control instead of jumping back to the tabs.
- Arrow keys, Enter and Space no longer fold a repo row you can't see after switching to Source control or Needs You, or hiding the sidebar.
- A repo row's ▶ no longer stays lit after the pointer leaves the window or the sidebar is hidden and shown again.
- A session's last output before it ends always reaches its scrollback before it shows as stopped, and discarding a session right after stopping it leaves no folder behind.
- A preset launch's Stop all also stops sessions that haven't shown up in the sidebar yet, and cancels a running launch even before any session exists.
- A Claude session's name no longer flickers with Claude's spinner, and a session titled only "Claude Code" shows its own label instead.
- A Claude session started in your shell that is lost before its history is saved now comes back in Recover as Claude, with its settings and conversation, not as a plain shell.
- The "Spawning session…" notice goes away as soon as the session starts, or when the spawn is refused or cancelled, instead of lingering for several seconds.
- Clicking a button on the left-hand rail no longer leaves its tooltip covering the panel it just opened.
- On a remote host the Shell dialog takes a folder in the host's own path style, and a folder that isn't a full path on the host is refused instead of opening somewhere else.
- Approve… and Deny on a device-pairing notification now work when pressed from Action Center, even with the app closed, and notifications show as rustling-tulip instead of PowerShell.
- While a pairing request waits, "Have a code?" lets you type a code made with `rustling-tulipd pair` on the host.
- A setting the daemon refuses in Settings now shows as unsaved in its own field, instead of being blamed on another change or re-sent with every later click.

### Changed

- When remote access has no certificate, Settings says why: the certificate command failed, its files were unusable, Tailscale gave no name, or the certificate task stopped.

## v0.3.0-alpha

### Added

- Sort repos with the ones that need you on top (the default), by latest activity, or in your own order.
- An All / Active switch in the sidebar header shows only the repos and workspaces with live sessions.

### Fixed

- The window, the taskbar and Explorer show the app's own icon instead of the Windows default.

## v0.2.0-alpha

### Added

- Remote mode over Tailscale: open another PC's sessions from a host picker that lists your tailnet machines and reconnects to the last host at start.
- A new device connects only after you approve it on the host PC with a one-time pairing code.
- Settings > Remote access turns remote access on and lists the devices, with who is connected, when each was last seen, and Block and Forget.
- When a remote host's session count changed since your last visit, the app offers to keep your layout, open all its sessions or start empty.
- Add repo and Browse take a typed path on a remote host.
- Ctrl+click a file path in a remote session's terminal, or use Fetch file…, to copy the file to this PC and open it.
- A fetched HTML page brings the images and stylesheets it links with it.
- A built-in text viewer opens a Ctrl+clicked file path in a tab with line numbers and syntax colours, scrolled to the named line.
- Markdown files open rendered, with a Rendered / Source toggle, working links and `#heading` anchors.
- The viewer finds text with Ctrl+F, copies whole selected lines, and re-reads a changed file when you come back to its tab.
- A setting opens text files in VS Code or your default app instead of the viewer.
- Claude sessions a reboot or crash killed resume on their own at the next start, in their old panes with their names and colours.
- Sessions that could not resume wait in Recover sessions, ticked, with a notice saying how many came back.
- A Claude pane stays a working shell when Claude exits.
- Header buttons in a session pane jump up and down between the prompts you sent.
- Copying a selection across Claude Code's boxed panels leaves out the borders.
- Drag a pane or a sidebar session onto a pane or a tab pill to split, swap or move it, and drag to reorder the sidebar.
- Settings sets each agent's launch command: another program, an alias or extra arguments.
- Choose the interface font in Settings: Geist (the new default), Inter, Schibsted Grotesk or any installed font.
- Keep awake is a three-way switch: off, while a session is busy, or always.
- A Sessions under repos setting shows every session, only the busy ones, or a count.

### Fixed

- Only the client you are typing in sets a shared session's terminal size, and resizing the window or folding the sidebar claims it.
- Keys typed while the client is busy reach the shell in order.
- Remote sessions close when remote access is turned off or its port changes.

## v0.1.1-alpha

### Fixed

- Restart runs an exited session again in its own worktree instead of a new branch.
- A restarted session no longer hangs on a blank pane.
- A session you stopped shows as stopped, not as exit code 1 in Needs You.
- A reattached session shows its shell's name instead of `rt-tracer`.
- Recover sessions shows other recent sessions when nothing else is listed.
- Sessions no longer inherit a parent Claude Code session's environment variables.
- The client starts without a console window.
- The empty main area, the pane margins and the exited-session overlay draw on solid backgrounds.

## v0.1.0-alpha

The first alpha: a Windows desktop client and a long-lived daemon that run many Claude sessions in parallel across repos and multi-repo workspaces.

### Added

- Start a Claude session or a plain shell in a repo's folder, or in a new git worktree on its own branch.
- Group repos into a workspace, where one session works across all of them with their relative paths kept; a VS Code workspace file is offered as one.
- Sessions run under a supervisor process each, so closing the window or restarting the daemon never kills them.
- Each session's status comes from Claude Code's hooks and its screen: working, asking you something, finished, idle, or idle with background work running.
- The Needs You panel lists every session waiting on you, and Windows notifications say when a session needs you or finishes.
- Tabs and split panes, with drag to reorder tabs, move panes across tabs, rearrange a tab into a grid, and undo a closed tab.
- The sidebar groups sessions by repo or by tab; add and remove repos and workspaces, and launch a repo's last settings again.
- Presets start many sessions at once from a prompts file, a folder or GitHub issue ranges.
- Exclude a session from busy tracking.
- Run a Claude session on DeepSeek with your own `DEEPSEEK_API_KEY`.
- Secret environment values you give a session are stored in Windows Credential Manager, never in a settings file.
- The terminal draws combining characters, every text attribute and OSC 8 links, and opens file paths and URLs with Ctrl+click.
- Each worktree's changes, staging, commit, discard, stashes and history, with side-by-side word-level diffs.
- Settings has a page per agent, notification choices, and starts the daemon when you sign in.
- A per-user installer that stops a running daemon before it copies files, leaving sessions alive.
