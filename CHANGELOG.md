# Changelog

What changed in each release of rustling-tulip, newest first. Installers are on the [releases page](https://github.com/leftos/rustling-tulip/releases).

## Unreleased

### Added

- Unbind every session of one machine, or rebind them into new tabs by a grid, side by side or stacked, or following that machine's own layout.
- A session's dispatch runs and subagents sit indented under it in the sidebar, a run's own subagents under the run, finished subagents until your next prompt, and a chevron on its row folds them away.
- Right-click the version in the status bar to check for updates at once; it says when you're up to date or why it couldn't check.
- A setting in Settings › General decides whether the first click on a pane that isn't focused also acts in it (on by default) or only focuses it.
- A pane following a dispatch run or a subagent shows its transcript the way Claude Code's terminal does: its replies, each tool call and the tool's output, which a click expands.
- The status bar shows CPU and RAM for This PC and any remote hosts you choose; right-click the pills to pick them, and a busy machine turns amber.

### Fixed

- Queued gates in the Gates panel show the command they will run, not the gate wrapper's own arguments.
- A followed dispatch run's recent actions no longer fill up with hook and thinking-token lines that pushed out its tool calls.
- Hovering a dispatch run or a subagent in the sidebar shows its full task name first, as a session's hover does.
- Moving a tab's last pane to another tab now shows that tab with the moved pane focused, instead of jumping to the first tab.

## v0.13.0-alpha

### Highlights

- Dispatch runs show in the sidebar tagged DS, and a pane can follow one with its cost, tokens, recent actions and a Stop button.
- The Dashboard lists the dispatch runs a session started under that session.
- A blue count on the taskbar, the tabs and the Sessions icon shows sessions that finished while you weren't looking.
- Ctrl+clicking a file path in a terminal opens it in its app at once.

### Added

- A blue count on the taskbar, the tabs and the Sessions icon shows sessions that finished while you weren't looking; looking at one clears it.
- Dispatch runs show in the sidebar under their repo, workspace or folder, tagged DS; clicking one opens a tab that follows it, dragging one onto a pane follows it there, and its menu stops it.
- A pane following a dispatch run shows its title, cost, tokens and recent actions with a Stop button, and keeps them with its run folder once the run ends.
- The Dashboard lists the dispatch runs a session started under that session, and its now line says when the session is waiting on one.

### Fixed

- A phone alert no longer goes out, or counts in the badge, for a session closed a moment before.
- One slow or unreachable push service no longer delays alerts to your other devices.
- Connecting or pairing from another device no longer waits while a phone saves its alert settings.
- The Claude accounts popover and Settings › Accounts line up every usage bar and percentage, whatever the limit's label.
- Ctrl+clicking a path ending in `#anchor` or `?query`, like `index.html#q1`, opens it, and a web page opens in your browser at that anchor.

### Changed

- Ctrl+clicking a file path in a terminal opens it in its app at once; Ctrl+Shift+click offers its app, the viewer or its folder.
- The tab you are looking at now stands out lighter than the other tabs, with an accent bar along its top.

## v0.12.1-alpha

### Highlights

- Settings' Install hooks, Accounts, Appearance and the Dashboard's stored key wait for OK or Apply, and Cancel leaves them as they were.
- With Claude's vim editor mode on, sessions show the right state and Chat answers land as text.
- Popovers, menus, dialogs and tooltips are wider, so their text fits on fewer lines.
- A session started with a first prompt on a cmd host, such as the phone's New session, stays open.

### Fixed

- On macOS, Go to pane on a toast brings the app to the front when another app is in front.
- A session started with a first prompt on a cmd host, such as the phone's New session, stays open instead of answering once and ending.
- The sidebar's Tabs view lists sessions from other computers shown in a tab, each with its computer's name beside it.
- On the phone's Home, starting Launch last or New session clears an earlier refusal of the other kind shown for that repo or workspace.
- A Claude session in bash mode whose reply quotes a question's footer reads as idle, not waiting on you, with no question in Chat.
- With Claude's vim editor mode on, a session running a background shell shows as background, and a narrow pane's question as waiting on you.
- A session whose subagent asks for permission keeps waiting on you, without a brief false Idle or a second alert before the dialog draws.
- With Claude's vim editor mode on, an answer typed from Chat into a question's own-answer field lands as text, even after an Esc.

### Changed

- Settings › Claude's Install hooks switch waits for OK or Apply, and Cancel leaves the hooks as they were.
- Settings › Dashboard's Clear is Delete stored key: it waits for OK or Apply and deletes only the shown provider's key; Test uses the applied settings.
- Removing a saved Claude account in Settings › Accounts asks you to confirm first.
- Settings › Accounts' names, order and auto-switch settings wait for OK or Apply, and Cancel leaves them as they were.
- Settings › Appearance and the Themes picker's Use preview at once but save only on OK or Apply; Cancel puts the saved look back.
- The Connections panel, context menus, confirm dialogs, the undo shelf, tooltips and Settings' host list are wider, so their text fits on fewer lines.

## v0.12.0-alpha

### Highlights

- Update another computer on an older release from the Connections dialog, over the tailnet or SSH, and tell your connected clients to update.
- Start sessions from the phone's Home: launch a repo's last launch again, a preset, or `/nextup` in any repo or workspace.
- Choose Claude's effort level when starting a session, with Fable among Claude's models.
- Questions, toasts, the Dashboard and Accounts show their whole text, wrapping it, instead of cutting it to one line.

### Added

- A session's screen on the phone shows its running subagents, background shells and next wake-up, as the desktop's strip does.
- A tab you haven't named shows the repos and workspaces of the sessions in it, everywhere its name appears; a tab you renamed keeps its name.
- An account's Fable 7-day limit shows beside its overall 7-day one in Accounts and Settings, whenever it is in use.
- The Dashboard shows each subagent's model and effort level after its name, such as "Opus 5.5 · high".
- Ctrl+clicking a file path in a terminal offers a menu: open it in its default app, open it in the viewer, or open its folder.
- On the phone's Home, Launch last starts a repo's or workspace's last launch again; a dropped connection never starts it twice or reports a launch that started as failed.
- The phone's answer sheet for a plan shows the plan itself above its buttons, scrolling on its own when it is long.
- Choose Claude's effort level, from Low to Max, when starting a session or as a default in Settings; Fable is offered among Claude's models.
- Claude's question about a new MCP server in a project shows as a card in Chat and on the phone, with its three choices as buttons.
- Add your own command-line arguments to a session in the spawn dialog's Advanced section; Duplicate and Launch last repeat them.
- On the phone's Home, New session starts `/nextup` in any repo or workspace, even one never launched before.
- On the phone's Home, Presets lists a repo's or workspace's presets and launches those that need no typing, showing each launch's progress.
- Connections and Settings show the release each host runs, and mark a host on an older release than this app as Behind.
- When another device updates this PC, the app counts down 10 seconds, says who asked, offers Cancel, then reopens after the update.
- Update a computer on an older release, over the tailnet or SSH, from the Connections dialog: confirm, and a notice follows it until it returns.
- From This PC's row in Connections, tell every connected client to update: each counts down 10 seconds with Cancel, and phones reload.
- Updating a host warns when the release ends its sessions or breaks the connection, and this app then updates itself right after the host.

### Fixed

- Ctrl+clicking a file path that runs onto a second line inside one of Claude's boxed dialogs opens the whole path, instead of only its first line.
- A plan, permission or question card shows in Chat as soon as Claude asks, instead of most of a second after the session turns to waiting.
- Answering a subagent's question in front of Claude's own no longer drops Claude's question for a moment, and a subagent's question answered before it showed stops showing as needing you.
- With Claude's vim mode on, a question whose cursor sits on "Type something" keeps showing as needing you, and its Chat card stays up.
- A prompt you answer or dismiss from the keyboard while subagents are busy now stops showing as needing you at once, and so does a turn you end with Esc.
- A session whose reply quotes a question's footer no longer shows as needing you, and pressing Esc on a question with such a quote above it now ends the wait.
- In a narrow pane, the branch name keeps a few characters before the session's name starts to shorten, instead of shrinking to a sliver.
- On the phone, a chat scrolled to the bottom stays there when the keyboard, the key bar or the Keys and Quick docks open.
- The phone's Home lists a group's sessions in the same order as the desktop each time it connects, instead of a new order after a relaunch.
- A session started with a first message in a folder Claude hasn't trusted yet waits until you answer Claude's question about trusting the folder, from Chat or the phone, then sends it, instead of closing Claude.
- Play in Settings › Notifications plays the sound you just picked, instead of the one saved before.
- When resuming an abandoned session fails, the phone shows why on that session's row.
- Keyboard shortcuts work in a window with no tabs before the first click, including right after choosing Start empty.
- Connecting to another computer no longer offers a layout choice that opened nothing; its sessions start in an empty layout.
- `rustling-tulipd spawn --repo` refuses a repo that belongs to a workspace, naming the workspace, instead of making a session filed under Detached.
- The phone shows a session with subagents or background shells running as Working in the background instead of Idle, and a starting, inactive, abandoned or orphaned session by name.
- A session you place in a pane takes that pane's size at once, instead of keeping its old size until the pointer moves over it.
- A session whose task name is too long for its pane no longer reads as finished mid-turn, so no false Finished notice fires.
- A question whose answers have previews no longer offers answer buttons in a toast or on the Dashboard that could only fail; it offers Go to pane.
- The What's new window shows a scrollbar when its notes run longer than it can show.
- A damaged settings file no longer copies its contents, which could include a secret, into the log.
- A click that focuses a pane or the window, or dismisses the jump palette, no longer reaches the program, so it can't answer a question.
- A settings file that isn't valid text keeps a copy instead of being overwritten, and a failed reset of your settings is tried again.
- A tap in the phone's terminal no longer drops a session out of Needs you while its question is still up.
- Keys typed into the phone's terminal while it can't send now show Offline on the session bar, as a chat send does.
- A phone app put away just as its host refused its pairing waits until it is opened again before it reconnects.
- Switching the phone's session screen to Terminal and back keeps the chat where you had scrolled it, instead of jumping to the newest row.
- While a reply streams on the phone, earlier answers keep their sideways scroll and any text you selected in them.
- A session left open in the phone's terminal no longer wakes the phone four times a second once it has loaded.
- The phone's Notifications switch no longer spins forever when the app's service worker never started; it says to reopen the app.
- The phone app opened offline right after installing it now loads styled and working, instead of as a bare page.
- A permission toast shows its whole command, wrapping it and growing to fit, instead of cutting it to one line.
- The Needs you list shows a session's whole question, wrapping it, instead of cutting it to one line.
- Accounts and Settings › Accounts show an account's whole email and reset times, wrapping them, instead of cutting them off.
- The undo shelf shows its whole message, wrapping it, instead of cutting it to one line.
- The Dashboard shows the whole of what a session is doing or waiting on, wrapping it, instead of cutting it to one line.
- A long footer status shows in full on hover, and long names in the jump palette and Themes end in "…" instead of cutting mid-letter.
- Unfocused panes dim alike whether their accent comes from the theme or a chosen colour, and each pane's header carries a fade of its accent.
- A question too tall for its pane can be answered from a toast or the Dashboard instead of being refused, and its Chat card shows the whole question.
- The Connections dialog's rows and headings no longer run together when the Add host form leaves them little room; the list scrolls instead.

### Changed

- After the client crashes, is ended from Task Manager or loses power, the next start asks whether to open your saved tabs or start without them.
- The Trusted checkbox is gone: choose Bypass permissions instead, and Launch last relaunches it without asking you to confirm; saved trusted launches carry over.

## v0.11.2-alpha

### Fixed

- Ctrl+clicking a path written in parentheses before a colon, such as `(X:\dev\page.html):`, opens the file instead of failing to find it.

## v0.11.1-alpha

### Fixed

- A Ctrl+click from another computer fetches a file from any git worktree of the session's repo, and says why when a path is outside them.

## v0.11.0-alpha

### Highlights

- The phone app opens each session as a chat, with a live terminal, a Needs you tray and notifications when a session needs you or finishes.
- Answer a question or permission prompt in your own words from Chat's cards or a toast.
- Ctrl+Shift+P opens a jump palette to go straight to any pane, tab, session, repo or workspace.
- A crash at start-up now says what happened, and the client always opens, closing your tabs only if it crashes twice in a row.

### Added

- Open a shell on any connected computer from the jump palette (Ctrl+Shift+P, "Open shell on …") or from its row in Connections.
- On Windows, a question or permission toast has a text box for your own answer: type it and press Enter, without opening the session.
- Paste an image with Ctrl+V, or drop files onto a terminal, into a session on another computer: they are copied there and their paths pasted.
- Your phone gets a notification when a session needs you or finishes, even with the phone app closed: turn it on from the Notifications button on the phone app's Home.
- Phone notifications wait while you're at your computer, and a session still waiting on you reaches your phone when you walk away; turn it off with "Hold while I'm at my computer".
- The phone app opens a session as a chat: read the conversation with its formatting, send messages, answer prompts from a bottom sheet, and send keys and quick replies.
- The phone's session screen has a Terminal toggle: the live terminal with a key bar (Esc, Tab, arrows, a sticky Ctrl, Quick), and plain shells open straight on it.
- The phone app's Home lists your sessions grouped as on the desktop, with a Needs you tray to answer prompts in one tap, and Stop and Resume on each session.
- The phone app's Home Screen icon shows how many sessions need you, kept up to date while the app is open.
- The phone app pairs itself: tap Ask to pair, approve the phone on your computer, then scan the code it shows or type it; the phone stays paired across launches.
- Ctrl+Shift+P opens a jump palette: type to find any open pane, tab, session, repo or workspace, and Enter goes straight there.
- A Ctrl+clicked file fetched from another host shows its size, progress and a progress bar in its toast, with Cancel until the download is done.
- Add repo accepts any folder: one that is not a git repo shows as DIR and opens sessions in place, with no worktree or branch choice.
- A narrow pane header moves its buttons from Pop out onwards into a ⋯ menu, so the session's name stays readable; Close stays put.

### Fixed

- A crash at start-up now says what happened, and a second in a row opens with your tabs closed and a copy of them kept.
- A damaged saved state now loses only the bad entries, keeping your other repos, workspaces and tabs.
- Chat and the phone show a Heads up notice's later steps as buttons, including the step after Dismiss.
- A session stays counted as Needs You while its prompt is up, whatever you click or press, until the prompt is answered or goes away.
- A question stays answerable when a tool beside it finishes, and the phone no longer says a question needs your permission.
- Phone Finished notifications no longer fire while a session is still working, thinking or running background work, and wait 5 seconds as the desktop does.
- Split down and Split right split the pane you asked for, even in a tab you rearranged, instead of adding a pane in the middle.
- The client no longer crashes when a Dashboard line or a label starting with a non-English letter is cut short with `…`.
- Answering a long question from a toast, the Dashboard or the phone's Home now works instead of failing with "the prompt is no longer up".
- Esc in desktop Chat interrupts a running turn, as it does in the terminal and on the phone.
- Restarting a session whose pane showed Chat or Summary brings the pane back on that view, not on the terminal.
- Summary names a turn started by a paste after its text, and no longer lists an interrupt as a turn of its own.
- Chat's plan card shows the plan's text when Claude's plan is too tall for the terminal to show.
- A tool result too long for Chat now ends with how many lines or characters were cut, instead of stopping mid-word.
- A prompt you send while Claude is busy shows below the turn it waits on, on the desktop and the phone, instead of above it.
- A reply streaming into Chat no longer breaks into stray paragraphs or gains blank lines inside its code blocks.
- After `/compact`, Chat shows the next reply once, with no row left stuck on "writing".
- A long paste in Chat shows as its text, without Claude Code's pasted-content tags around it.
- Chat answers keep their single line breaks, so a list of lines no longer runs together into one paragraph.
- A tool with nothing to summarise is named once in Chat, not twice, and a PowerShell call shows its command.
- Settings › Remote access stops showing a phone as Connected as soon as its connection ends, including when you switch Remote access off.
- Chat reads Claude's permission prompt in a named session again, so it shows the prompt's card instead of "Can't read the prompt box right now."
- Allow from the phone works on an Edit or Write prompt, and Deny still works when Claude leaves stray characters on the No option.
- Pasting an image into a Claude pane on Windows works: with an image on the clipboard, Ctrl+V sends Claude its own image-paste key.
- A window connected to another computer shows that computer's Claude accounts in Settings › Accounts and the footer, and can switch them; adding, removing and renaming stay on the computer that runs them.
- A right-click in a terminal pastes this computer's clipboard, even in a session on another host; Shift+right-click still reaches programs that use the mouse.
- The Gates view and its footer pill show only on a computer with the gate installed, not on one where only old gate files remain.
- A Nushell pane's command dots sit on each prompt's own row, so no stray dot is left after `clear`, and Copy command, Re-run and the prompt jumps work there.
- The Connections dialog shows each host's details whole, wrapping between them instead of squeezing "4 sessions" into a column one letter wide, and a failed tailnet scan's error sits on its own lines under its heading.
- A long folder path in Open a shell, or long text in any one-line field, stays inside its box and scrolls to follow the cursor instead of running under Browse.
- Clicking a Needs you or Finished toast, the Windows notification or the app's own, now shows that pane's tab and gives the pane keyboard focus.
- A session whose agent exits back into its shell keeps its name in the sidebar instead of switching to the shell's folder.
- The account login session reads "Log in a new account" in the sidebar, not its run id, and has no Add repo banner.
- A session whose agent has exited no longer shows the green waiting ring or counts as waiting in the footer.
- Once an agent started by hand in a shell exits, `rustling-tulipd sessions` shows a plain shell again and the pane drops the agent's title.
- A Claude started by hand in a shell, or a session whose transcript folder is still new, no longer fills the daemon log with errors and warnings.

### Changed

- Session names in the sidebar are smaller than their repo or workspace name, so the name that holds them reads as the heading.
- Chat's question, plan and permission cards each have a text box for your own answer, replacing the composer's "Yes, and tell…" and "No, and tell…" buttons.
- A background tab's right-click menu no longer offers Rearrange panes; the active tab and popped-out tabs still do.
- A toast shows its whole question and every answer, one per row with Claude's description under each, and grows to fit them.
- Toasts and notifications name the session's repo or workspace beside their title, so you can tell which project is asking.

## v0.10.2-alpha

### Fixed

- In a wide Chat pane, every message after a very long one, such as a pasted build log, can be scrolled to.
- On macOS, a Mac you approved for remote access stays approved after the app quits or updates, instead of asking to pair again.

## v0.10.1-alpha

### Fixed

- On macOS, an app opened from Finder or the Dock lists your tailnet's devices in Connections, instead of an error that Tailscale's output does not parse.

## v0.10.0-alpha

### Highlights

- rustling-tulip now runs on Apple silicon Macs, as a signed and notarized .dmg that updates itself.
- Save several Claude logins, see each one's usage in the footer, and switch accounts by hand or let auto-switch do it.
- On Windows, a Gates view shows this PC's gate slots, queue and recent runs, and sets each pool's slot count.
- Pick each host's default shell, and a Claude you start by hand in a shell gets Chat and Summary too.

### Added

- rustling-tulip now runs on Apple silicon Macs, as a signed and notarized .dmg that updates itself.
- On Windows and macOS hosts, the footer shows the Claude account in use and its usage; click it to switch accounts, set auto-switch and see when they run dry.
- Settings › Accounts lists your saved accounts to switch, rename, reorder, remove or add, and sets auto-switch's threshold, cooldown, strategy and forecast window.
- Settings › Accounts can log in a new account: the sign-in runs in a terminal session in the sidebar, and the account joins the list once it ends.
- A toast tells you when auto-switch changes accounts, finds every account used up, or stops; Settings › Notifications › Account switches turns them off.
- On Windows, a Gates view on the rail and a footer pill show this PC's gate slots, waiting gates and queue, and set each pool's slot count.
- The Gates view lists the last two hours' gated runs with how each ended, and the last seven days' most frequent passed commands.
- Each gate row in the Gates view shows its command's usual time beside its own, and the footer's Gates pill names a run that failed in the last ten minutes.
- Settings › General picks each host's default shell for new sessions, from its installed shells or a custom command.
- The new-session dialog's Target list narrows as you type, matching repo and workspace names in part, so "ya" finds yaat.
- With Nushell or fish as the default shell, new Claude sessions start inside it, like PowerShell, cmd and POSIX shells.
- Settings › Claude can install the app's hooks in Claude Code's user settings, so a Claude you start by hand in a shell reports its conversation and status.
- A plain shell where you start Claude yourself, by name, an alias or a wrapper like a pwsh function, gets Chat and Summary while Claude runs.
- On macOS hosts, programs that ask the terminal for its colours, such as bat and delta, get your theme's, so they pick light or dark styles to match.
- On macOS, Restart to update replaces the app in place and relaunches it, and the old copy is removed at the next launch.
- On macOS, Needs you and Finished notifications show as the app's own toasts at the bottom right, without taking focus.

### Fixed

- On macOS, Cmd+C and Cmd+V copy and paste in terminals and text fields, and Ctrl+C always interrupts the program.
- On macOS, remote access finds the Tailscale app's command-line tool, or Homebrew's, when the app is opened from Finder or the Dock.
- Settings › Remote access's "Start the daemon when I log in" waits for OK or Apply like every other setting, and Cancel leaves it as it was.
- On macOS, an app opened from Finder or the Dock finds Claude, node, git and gh on your login shell's PATH.
- Opening a subagent's view in Claude no longer raises a false Finished notification or drops the session's background-work ring.
- Chat no longer flashes "The prompt box has a draft" right after you send a message.
- Dragging a remote host's session when that host drops now shows at once that the tabs and panes won't take it.
- The Connections dialog wraps a long tailscale error and a saved host's session count instead of cutting them off.
- Settings' keep-awake row names the selected host instead of always saying "this PC".
- With several hosts in the sidebar, This PC's "No repos or workspaces yet" note sits under This PC's header, not above every host.
- The new-session dialog's Target list keeps its highlighted row in view, and a click inside the list no longer closes it.
- Ctrl+click opens a whole terminal path that holds spaces, or wraps inside a themed Claude reply box, once the file is found.
- A toast with a long title now wraps inside its card, so its close button stays reachable.
- A pane header's tooltips now open below the header, so they no longer cover its Terminal, Chat and Summary buttons.
- In a narrow sidebar or at a large text size, the sidebar header's All/Active and Repos/Tabs switches move below the title instead of running past the edge.
- Fetch toasts name an SSH host as you typed it, and a click on a path link shows its toast at once.
- A folder fetch's progress keeps moving while it finishes, drops Cancel once it can't stop, and stays closed when you close it.
- A link to a repo root missing on the host now says the folder is not there, not that it is outside the repo.
- A connecting host's sidebar header no longer runs "Connecting…" into + Repo, and the sidebar's view switcher fits at the largest text size.
- At the largest text size a pane header's provider, host and +N chips shorten before the pane title does.
- Settings' host list has its own HOST heading, and an offline host is dimmed like it is in the sidebar.
- Text fields in dialogs and menus sit on the dialog's lighter inset, and the side rail, sidebar launch and tab close icons brighten on hover.
- A remote host's Claude sessions keep their titles while the host reconnects, instead of showing "Claude Code".
- Italic terminal text at the Medium and SemiBold weights now matches the weight of the text around it in Geist Mono, JetBrains Mono and Cascadia Code.
- A session you left in Chat or Summary reopens there when a duplicate host is dropped and its panes move to the host that runs them.

## v0.9.0-alpha

### Highlights

- Ctrl+click a folder path in a remote host's terminal to fetch the whole folder, or fetch a fetched file's whole folder from its notice.
- Settings has a search box that finds any setting, and OK, Apply and Cancel so changes wait until you confirm them.
- The chat stays a readable width, sets each final answer apart, and folds runs of tool calls into one line in Compact mode.
- The installer and the app are signed with a verified publisher, and update notices show what's new since your version.

### Added

- The Windows installer and the app's programs are signed with a verified publisher, so Windows names who made them.
- A What's new button on the update notices shows the release notes for every version since the one you last had.
- A file name after a full path on the same terminal line opens from that path's folder, so `d.html` after `x:/a/b/c.html` opens `x:/a/b/d.html`.
- Settings > General has a Chat section to pick Compact or Comfortable and set the chat's maximum width.
- The chat column stays a readable width on wide panes, and each turn's final answer is set apart in its own box.
- The chat's new Compact mode, on by default, folds a run of reads, searches and commands into one line you can open, like the terminal does.
- Settings > General has a Diagnostics section with a Detailed logging switch that records why sessions change status, and an Open logs folder button.
- Settings has a search box that finds a setting on any page, with match counts per page and the match highlighted; Ctrl+F jumps there.
- Ctrl+clicking a folder path in a remote host's terminal fetches the whole folder, with progress and Cancel, leaving out `.git`, `node_modules` and caches.
- A fetched remote file's notice offers Fetch whole folder for its folder, and a fetched folder's notice offers Reveal to show it in Explorer.

### Fixed

- The new-session dialog keeps its fields, agents and worktree preview right when its target moves between hosts.
- A remote host's preset wizard no longer closes when This PC reconnects, and closes when its own host goes offline.
- A PC whose app data was copied from another machine no longer shows up as "Same daemon as" that machine.
- When Stop can't stop the background service, the app now says why instead of acting as if it stopped.
- The new-session dialog greys out a host the moment it goes offline and says "{host} is offline" for it, without losing what you typed.
- A remote host no longer flashes "No repos or workspaces yet" in the sidebar while it is still connecting.
- A remote repo's menu now says Explorer and VS Code aren't available for a remote host, instead of saying it has no folder.
- Settings no longer offers Manage worktrees while a remote host is chosen, where it opened This PC's worktrees instead.
- A mistyped `rustling-tulipd` command now prints a usage error instead of starting the daemon, and `rustling-tulipd -V` prints its version.
- Stopping the daemon as administrator after an administrator daemon crashed now clears its leftover handshake instead of refusing to stop.
- Retiring or stopping the background service never force-closes a program that doesn't own the service's connection.
- Reconnecting to one host no longer drops another host's worktree cleanup prompts or running preset launches.
- A remote host's layout question closes as soon as another window answers it.
- Forgetting a host in one copy of the app no longer closes that host's panes in another copy connected to the same machine.
- Ctrl+Left and Ctrl+Right now move a word at a time, and other Shift, Alt and Ctrl combinations with arrows, Home, End and F-keys reach programs.
- Ctrl+Backspace now deletes the previous word in a session, as it does in Windows Terminal.
- A Claude session with an MCP task or a monitor still running no longer shows as Finished, just as with a background shell.
- When the app's background service fails to start, the app now shows the reason instead of only saying it timed out.
- A session that fails to start no longer leaves its agent or shell running unseen in the background.
- Pop-out windows keep their places and follow their sessions correctly when several connections reach the same machine.
- The new-workspace dialog no longer closes or shows an error because of another host's reply, and a remote session's name uses its own host's agent names.
- An SSH host given by an IPv6 address keeps its pinned repo and collapsed panels in Source control.
- An administrator daemon now keeps running the session helper it started with, so a file swapped into the install folder later never runs as administrator.
- A remote repo's menu, Launch last, presets, Stop and the delete-worktree dialog now act on that host even when This PC has a repo or session of the same name.
- A remote session's sidebar row, Needs you row, notices and close dialog now act on that host when This PC has a session with the same id.
- An SSH connection that starts the background service now closes as soon as it ends, and `rustling-tulipd` output redirected to a file is no longer lost.

### Changed

- Settings now has OK, Apply and Cancel: your changes wait until you press OK or Apply, and Cancel or Esc discards them.

## v0.8.1-alpha

### Fixed

- A newly started Claude session no longer shows as Finished, or raises a Finished toast, before you have used it.

## v0.8.0-alpha

### Highlights

- The Dashboard shows every running agent at a glance and answers their questions and permission prompts with one click.
- On Windows, the app's own toasts answer a prompt or go to its pane without taking focus, with softer chimes.
- `rustling-tulipd` drives sessions from a shell: list, spawn, send, read, wait and watch.
- Sessions run as administrator are kept apart: programs without administrator rights can no longer start or reach them.

### Added

- The Dashboard opens on a Compact view: every running agent by repo, with its state, work and subagents, and no scrolling.
- Answer a session's question or permission prompt from the Dashboard with one click, or jump to its pane.
- Click a session in the Dashboard's By status view to open a drawer with its running summary, first prompt and recent work.
- Settings has a Dashboard page: turn session summaries on, pick the provider, store its key, set a daily cap and test it.
- On Windows, Settings › Notifications › Show toasts as can switch to the app's own toasts, which answer a prompt or go to its pane without taking focus.
- Finished toasts play the app's own sound instead of Windows', with their own chime, .wav or .ogg, or none, and stay silent during Do Not Disturb.
- Pick each toast's sound from a list of chimes in Settings › Notifications; the defaults are new, softer mallet chimes.
- Choose how long a Finished toast waits before it shows in Settings › Notifications, from none to 20 seconds.
- The Dashboard header shows today's cost, tokens and commits across every session, kept through daemon restarts.
- The Dashboard shows each session's last two hours as a bar with a short caption, beside its ten-minute activity.
- `rustling-tulipd` can drive sessions from a shell: `sessions`, `read`, `spawn`, `send`, `kill`, `wait` and `watch`, each with `--json`.
- The Dashboard's Issue column shows the Linear key named in a session's branch.

### Fixed

- Dragging the sidebar's edge no longer jumps it past the pointer; the edge now follows the mouse.
- A daemon running as administrator no longer lets programs without administrator rights start or reach its sessions; they are told to run as administrator.
- Sessions a remote client starts over SSH on a Windows host where you are signed in no longer run as administrator, and can use saved keys.
- A session started while no pane shows it, from a preset, auto-resume or another device, now starts instead of hanging.
- A running app checks for updates every hour instead of once a day, and Settings offers Restart to update as soon as a check finds one.

### Changed

- Session summaries stay off until you turn them on in Settings › Dashboard, since each one uses your provider's account; a saved choice is kept.
- A new window opens straight away when there is nothing to choose, instead of asking how to start with Start empty as the only option.
- Sessions started as administrator now keep their own settings, history, sessions and saved keys, separate from and starting without your usual ones, which stay where they are.

## v0.7.2-alpha

### Fixed

- A Finished toast no longer fires when a Stop hook sends the agent back to work; the session stays Working until its hooks finish.

## v0.7.1-alpha

### Fixed

- Finished toasts no longer fire after nearly every tool call while Claude works through a task-list item.
- A Finished toast now waits 5 seconds and is dropped if the session starts working again.

## v0.7.0-alpha

### Highlights

- Tail a log a chat command writes, in a scrolling box under its card that follows the file as it grows.
- Hear when an agent needs you, and on Windows see the Needs you count as a badge on the taskbar button.
- Start sessions faster: a Spawn submenu on every repo and workspace, a target dropdown, and a remembered Trusted choice.
- A quieter app: toasts and banners no longer narrate what you can already see.

### Added

- Right-clicking a repo or workspace offers a Spawn session submenu: Last, each agent, Shell and your presets.
- Opening a Bash or PowerShell card in the chat shows the whole command, one statement per line.
- The footer shows the app's version at its right end; hover it for the daemon's.
- A sound plays when an agent needs you: a bundled chime, your own .wav or .ogg, or none, set in Settings › Notifications.
- A chat command that writes a log offers Tail under its card: a scrolling box that follows the file as it grows.

### Fixed

- The app checks for updates as soon as it starts instead of 30 seconds later.
- The footer's status text shows only problems, not the focused session's name and id.
- Once Claude exits back to its shell, a session's Chat and Summary turn off and its pane shows the terminal, and they come back when Claude starts again.
- Text typed in a chat's message box survives a reconnect to its host.
- Chat question cards show the whole question, each answer's description and the question's header.
- Panes reopened after quitting with sessions kept running, or moved to another tab, draw at their new size at once instead of garbled until a key is pressed.
- An agent icon a remote host sends is drawn only when it is a plain, self-contained SVG; any other shows the generic glyph.

### Changed

- Sessions show their program as an icon (Claude, PowerShell, cmd, bash) in the sidebar, pane headers and spawn dialog, and Trusted as a shield.
- The New session dialog remembers your last Trusted launch choice.
- With Trusted launch by default on, the New session dialog skips its trusted warning and Launch last of a trusted session no longer asks for review.
- The New session dialog picks its repo or workspace from a dropdown instead of a wrapped list.
- Ctrl+click on a file in the terminal opens it in its default app and Ctrl+Shift+click in the viewer; the Settings switch for this is gone.
- Toasts and banners stop narrating what you can already see: no toast when an agent exits to its shell or a fetched page opens, and shorter spawn and launch notices.
- On Windows the Needs you count shows as a badge on the app's taskbar button instead of in the window title.

## v0.6.1-alpha

### Added

- A Frame width setting sets the coloured frame round the terminal from off to 6 px, for the app, a repo, a workspace or one session.

### Changed

- The coloured frame round a session's terminal is 1 px wide by default instead of 6 px, and the text keeps its place.

### Fixed

- A Claude session working on a task-list item no longer shows as idle mid-turn or sends a false Finished notification.

## v0.6.0-alpha

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
- The app connects to this PC and every saved host you switch on at once, keeping each host's sessions, alerts and panels; the sidebar groups them by host, each with its own + Shell, remote panes and tabs name their host, and an offline host dims.
- Spawning, shells, Add repo, presets and Settings work on any connected host: the spawn picker groups repos by host and Settings has a host selector.

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
