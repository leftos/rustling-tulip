# rustling-tulip

rustling-tulip is a Windows desktop app for running many Claude Code sessions side by side, across your repos and multi-repo workspaces. A background daemon owns every terminal, so sessions keep running when you close the window and come back after a reboot. Each session can get its own git worktree and branch, and the sidebar shows which sessions are working, which have finished and which need you. It runs the `claude` CLI you already have; it never calls the Anthropic API itself.

It is alpha software, Windows only for now.

![The main window: sessions grouped by repo in the sidebar, two session panes in a tab, and the status footer](docs/images/sessions.png)

## Features

### Sessions and the sidebar

- Start a Claude session or a plain shell in a repo's own folder, or in a new git worktree on its own branch, so parallel sessions never edit the same checkout.
- Group repos into a workspace: one session works across all of them, with their relative paths kept. A VS Code `.code-workspace` file next to a repo is offered as a workspace.
- Every session's status has its own shape and colour: working, asking you something, finished while you were away, idle, or idle with background work still running.
- The Needs You panel lists every session waiting on you, longest wait first, and a Windows notification fires when a session needs you or finishes (skipped when you are already looking at it).
- Tabs and split panes: drag a pane or a sidebar session onto a pane to split, swap or move it, rearrange a tab into a grid, and undo a closed tab or a moved pane.
- The sidebar groups sessions by repo or by tab, in Comfortable or Compact rows, and can show every session under a repo, only the busy ones, or just a count.
- Repos sort with the ones that need you on top, by latest activity, or in your own order, and an All / Active switch hides the repos and workspaces with nothing running.
- Launch again with one click: a repo's last launch settings are replayed from its row. Presets start many sessions at once from a prompts file, a folder or GitHub issue ranges.
- In the terminal, copying from Claude Code's boxed panels leaves out the borders, file paths and URLs open with Ctrl+click, shell commands get a pass/fail mark in the gutter with copy and re-run, and two header buttons jump between the prompts you sent.
- When Claude exits, its pane stays a working shell.
- Secret environment values you give a session are stored in Windows Credential Manager, never in a settings file.

### Remote access over Tailscale

- Turn on remote access in Settings and your other PCs on your tailnet can open this PC's sessions. The daemon serves only your tailnet, over TLS with a Tailscale certificate.
- The host picker lists your tailnet machines and the hosts you saved, and the app reconnects to the last host when it starts.
- A new device has to be approved on the host PC with a one-time pairing code. The Devices list shows who is connected and when each device was last seen, and Block cuts a device off until you forget it.
- Ctrl+click a file path in a remote session's terminal, or use Fetch file…, to copy the file to this PC and open it. An HTML page brings its images and stylesheets with it.
- When a host's session count changed since your last visit, the app offers to keep your layout, open all its sessions or start empty.

![The source control panel beside the session panes: staged and unstaged changes by folder, stashes and history](docs/images/source-control.png)

### Source control and diffs

- Each worktree's changes, staging, commit, discard, stashes, history and commit details, with a link to the commit on your forge.
- Side-by-side diffs with word-level changes, syntax colours, an overview ruler and F7 / Shift+F7 to step through changes.
- Deleting a worktree deletes its branch only when its commits have already landed; otherwise it asks, branch by branch.

![A side-by-side diff of a Rust file with word-level changes and the overview ruler](docs/images/diff.png)

### Text and Markdown viewer

- Ctrl+click a file path in the terminal and it opens in a read-only viewer tab with line numbers and syntax colours, scrolled to the line the path names.
- Markdown files open rendered (headings, lists, task boxes, tables, code blocks), with a Rendered / Source toggle. Links open, and `#heading` links scroll to the heading.
- Ctrl+F finds text, a click or drag selects whole lines to copy, and a tab re-reads its file when you come back to it and the file has changed.
- A setting sends text files to VS Code or your default app instead.

![A rendered Markdown file in a viewer tab, with the Rendered / Source toggle and its header buttons](docs/images/markdown-viewer.png)

### Recovery after a reboot

- Sessions run under a small supervisor process each, so closing the window or restarting the daemon never kills them.
- After a reboot or a crash, Claude sessions resume on their own at the next start (`claude --resume`) in their old panes, with their names and colours, and a notice says how many came back.
- Anything that could not resume waits in Recover sessions, which lists the last 7 days of ended sessions with the lost ones ticked.

### Settings

- A page per agent: Claude's permission mode, its provider (Anthropic, or DeepSeek with your own `DEEPSEEK_API_KEY`), and its launch command, so you can run another program, an alias or extra arguments.
- Trusted launch by default, which notifications to show and when to skip them, and whether to keep the PC awake (off, while a session is busy, or always).
- Start the daemon at login, the folder worktrees go in, and Manage worktrees to clean up old ones.
- Appearance: session accent colours, terminal and interface fonts (Geist, Inter, Schibsted Grotesk or any installed font), font sizes per session or per tab, and the window title.

![The Settings window on its General page](docs/images/settings.png)

## Install

1. Install [Claude Code](https://github.com/anthropics/claude-code) and check that `claude` runs from a terminal: rustling-tulip finds it on your `PATH`.
2. Download `rustling-tulip-<version>-setup.exe` from the [latest release](https://github.com/leftos/rustling-tulip/releases/latest) and run it. It installs for your user only, with no administrator prompt, and can start the daemon when you log in.
3. The installer is not code-signed yet, so Windows SmartScreen warns on first run: choose More info, then Run anyway.

Remote access also needs [Tailscale](https://tailscale.com/) on both PCs, signed in to the same tailnet, with HTTPS certificates turned on for it.

## What's new in v0.3.0-alpha

- Sort repos with the ones that need you on top (the default), by latest activity, or by hand.
- An All / Active switch in the sidebar header shows only the repos and workspaces with live sessions.
- The app has its own icon in the window, the taskbar and Explorer.

The full list is in the [release notes](https://github.com/leftos/rustling-tulip/releases/tag/v0.3.0-alpha).

## Building from source

See [docs/README.md](docs/README.md) for the layout, build commands and developer docs.

## License

MIT.
