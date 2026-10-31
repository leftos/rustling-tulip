# Changelog

What changed in each release of rustling-tulip, newest first. Installers are on the [releases page](https://github.com/leftos/rustling-tulip/releases).

## Unreleased

### Added

- Settings › Appearance opens a Themes window: pick a built-in or your own theme from a live preview with its contrast check.
- The Themes window's editor changes any colour of a copied theme, shows its contrast as you go, and can jump to a colour by clicking its preview.
- The Themes window exports a theme to the clipboard and a file, and imports a theme file as one of your own.
- A UI weight setting in Settings › Appearance draws the interface in Regular, Medium or SemiBold, applied at once.
- A terminal weight setting draws terminal text in Regular, Medium, SemiBold or Bold for the app, a repo, a workspace or one session, in place of the Bold checkbox.
- A UI text size setting in Settings › Appearance makes the interface's text 2 px smaller to 4 px larger, applied at once.
- `rustling-tulipd screen <session id>` prints how the app read a session's screen, rule by rule, with the screen itself, and `--fixture <path>` saves it.
- A Claude session's pane can show its conversation as a chat, with answers appearing as they are written, through the pane's Chat button, its menu or Ctrl+Shift+M; each session remembers which view it was last shown in.
- A chat pane shows what is still running under the chat: its subagents, background shells and the next wakeup, with their ages.
- A chat pane answers Claude's prompts from cards, by click or key, and sends your messages from a box under the chat, keeping your text when a send fails.
- A pane can show a session as a summary, one line per stretch of work that opens into its messages, through the header's Summary switch or Ctrl+Shift+U.
- Chat and summary panes draw stretchtext replies as expandable text: a click opens a short phrase into its longer form, one level at a time, and `‹` closes it.
- When Claude asks for you while a subagent's view is open in the terminal, the session switches back to the main conversation, setting any draft aside and restoring it afterwards.
- The app connects to this PC and every saved host you switch on at once, keeping each host's sessions, alerts and panels; an offline host dims.

### Fixed

- Sessions running when you sign out, restart or shut down Windows are resumed automatically on the next start, instead of waiting in Recover.
- A session on a branch whose worktree folder you deleted by hand now starts, and Recreate from base works there, instead of failing with "already used by worktree".
- Deleting a locked worktree from Manage worktrees or a session discard now removes it completely, so its branch can be used again.
- After a restart, the app no longer mistakes another program that reused a session's process number for that session, so it can no longer stop or reattach the wrong one.
- A phone or laptop that drops off the tailnet without closing its connection is now disconnected within a minute, and a slow one no longer makes the host buffer every session's output without limit.
- Repos that store refs in git's reftable format now refresh their status and stash list after a commit, branch switch or stash.
- A long-running session's history now starts at a line when reopened, instead of mid colour code with stray characters at the top of the pane.
- A power cut or a sleep the PC never wakes from no longer leaves sessions, settings or saved hosts as empty or half-written files.
- Sessions no longer end when the terminal, IDE task or tool that started the app closes and takes its child processes with it.
- After a restart, or when Claude's hooks don't run, a session still running a background shell or subagent is no longer reported as finished.
- A worktrees folder that can't be created in Settings now shows why, such as "Access is denied".
- A session started just before the app's background service crashed or the PC restarted is no longer lost: it is recovered on the next start.
- An error that arrives while a repo's menu loads its presets now shows its own message, instead of marking the presets as failed to load.

### Changed

- The interface uses Inter by default, which reads better at small sizes; Geist is still in Settings › Appearance.
- The window and its terminals draw in the softer Nord colours by default, instead of the near-black look.
- The app-wide accent and terminal background now come from your theme; colours you set before become your theme "My colours".
- In Appearance, the first accent swatch is now "Theme" and the "Default" background follows the theme: picking either clears that level's colour.
- The Connections picker groups this PC, your saved hosts and the devices on your tailnet, shows each host's state, and adds a tailnet or SSH host from one form.
- Tab pills and the window title count the sessions that need you, showing nothing at zero; Settings › App title brings back busy/total or shows idle/total.

## v0.5.2-alpha

### Added

- The app checks for new releases, downloads and verifies them, and offers Restart to update; your sessions keep running through the update.

### Changed

- A higher-contrast look: menus, popovers and dialogs stand clearly above a near-black window, session rows are cards, and menu groups are ruled.

## v0.5.1-alpha

### Fixed

- A host's daemon started by an SSH connection no longer stops, taking its sessions with it, when that connection ends.
- A daemon started from an SSH login says it cannot reach Credential Manager, and how to start it so saved secrets work, instead of treating secrets and summarizer keys as missing.

## v0.5.0-alpha

### Added

- Connect to another Windows PC over SSH, without Tailscale: Add SSH host… in the Connections picker takes an alias or user@host, and Test connection checks it first.
- The installer puts its folder on your user PATH, and uninstall takes it off, so `rustling-tulipd` runs by name from a terminal or an SSH login.
- Open a repo's, workspace's or tab's menu from the keyboard with Shift+F10 or the Menu key, then pick a row with the arrows and Enter.
- Terminal programs that ask for urxvt or pixel-precise mouse reporting now get clicks, drags and the wheel in the format they asked for.
- A Dashboard button on the activity rail opens the Dashboard as a tab, one per window, with a count of sessions waiting on you.
- While the Dashboard is open, a working session's summary line refreshes every 3 minutes until its turn ends.
- Each finished turn of a Claude session is summarized in one line, kept across daemon restarts; a turn that ends asking you something lists the session under Needs you.
- A Finished notification is titled and worded from the turn's summary when one arrives within a few seconds, and a Needs you notification names the question or permission it waits on.
- The Dashboard tab lists every agent session by status: what each is doing now, its context, recent activity, cost and running subagents, with a queue of the sessions waiting on you.

### Fixed

- The Settings window keeps one size whichever category you pick, and scrolls a long one.
- A trusted Launch last again whose dialog cannot open, because another dialog is up, now says why instead of doing nothing.
- A repo or workspace menu disables Open in Explorer and Open in VS Code when the folder or workspace file is gone, and says so.
- The installer stops a running daemon only after it proves it is the real one, so the daemon's token never goes to another program listening on its port.
- Dropping a pane back where it already was no longer stops the tab arranging itself automatically.
- A watched VS Code workspace file edited while the daemon was down is applied when it starts.
- Looking for VS Code workspaces on a slow or offline folder no longer stalls typing and other actions until the search ends.
- A preset launch's progress toast no longer sticks at "Launching" when the app falls behind on updates.
- When a preset's script fails, the wizard marks the script that failed, and Esc no longer closes the wizard while its scripts are still running.
- A preset that sets its panes per tab to 0 is refused with an error naming the preset, instead of opening every session in its own tab.
- An agent that starts through a `.cmd` file refuses to launch, saying why, when a prompt, path or setting holds a character cmd would run or rewrite.
- A settings file that fails to load is kept beside a fresh one instead of being overwritten, and its saved secrets are not swept away.
- The daemon's and the app's logs no longer grow without limit while they run: each rolls over at 5 MiB.

## v0.4.0-alpha

### Added

- The Needs You panel says what a waiting session asks: its question, the tool it wants to run, or the plan to approve, cut short when it runs long.
- On a remote host the preset wizard takes typed paths on that host for its prompt file, prompt folder and path variables.
- Pop a pane, a whole tab, a diff or a session out into its own window; Dock back returns it to the main window.
- Pop-out windows reopen where you left them the next time the app starts.

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
