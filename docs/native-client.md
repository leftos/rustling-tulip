# Native client

The desktop client is `apps/native` (binary `rustling-tulip-native`): GPUI (Zed's UI framework) with `alacritty_terminal` for the terminal grid. It replaced the Tauri/WebView2 app, which is retired and kept as history on the `tauri` branch (tag `tauri-last`). The daemon, tracer and `crates/protocol` are shared and unchanged by the client.

This doc records the decisions a change to the client needs to know; what each feature must do is in the [parity checklist](./plans/native-client-parity.md), and open work is in [plans/MAIN.md](./plans/MAIN.md). How the pieces fit together: [architecture.md](./architecture.md).

## Stack

- **GPUI 0.2.2 + `alacritty_terminal` 0.26.** The client moved off the web stack; GPUI draws natively and `alacritty_terminal` supplies a mature VT parser and grid. Iced is the fallback if GPUI's API churn becomes a problem.
- **Crate rules.** `apps/native` is a main-workspace member under the workspace lints, prek clippy, `cargo nextest run` and `cargo deny check`. `main.rs` is a thin binary over the library `rustling_tulip_native`, so specs link the library.
- **Testable core, thin view.** State a feature adds (sidebar tree, split tree, tab list, key mapping, span building, forms, dialogs' focus rings) lives in plain-Rust modules with unit tests; the GPUI `*_view.rs` renders it and forwards events. Every feature mounts into `RootView` (`lib.rs`), the hotspot concurrent items rebase over.
- **Clipboard** goes through GPUI's own API (no `arboard`).
- **Folder picker**: `folder_picker.rs` is the client's own Windows folder and file dialog, copied from GPUI 0.2.2's `file_open_dialog` (Apache-2.0) because GPUI's `PathPromptOptions` has no start folder. It adds `SetFolder`, so Add repo opens at the folder the last added repo sits in (`last_repo_dir`), Shell… at its typed folder or the quick-shell folder, and the worktrees Browse at its field.

  Its file mode filters to the file types the caller names (a `FileFilter`): `.code-workspace` for the workspace creator's Browse, which remembers its own folder (`last_vscode_dir`) separately from the repo folder, and `.txt` / `.md` or a variable's own extensions for the preset wizard's pickers.

  A start folder is checked off the UI thread and dropped when it is missing or takes over 2 s (an offline share). Off Windows it falls back to GPUI's picker with no start folder.
- **Repos and the no-repos states**: + Repo ("Add repo…") and + Workspace sit in the sidebar toolbar's `⋯` menu beside Shell…; the 30 px row stays + Session, + Shell, `⋯`.

  With no repos or workspaces the sidebar shows "No repos or workspaces yet." over an Add repo button, the main area's empty state gains Add repo beside Spawn a session and Open shell, and the spawn dialog (Ctrl+Shift+N, or any entry without a locked target) opens in a no-repos state with Add repo and Spawn disabled, turning into the form on the first repo that arrives; + Session and Spawn a session stay disabled. `AddRepo` carries no `request_id`, so a refusal is the generic error toast.

  Right-clicking a repo's or workspace's row opens its container menu, whose last row, below a separator, is a danger `Remove repo` / `Remove workspace`. With no live session the first click arms the row (`Confirm: remove repo…` / `Confirm: remove workspace…`) and the second unregisters.

  With live sessions the first click opens `Remove repo?` / `Remove workspace?` instead, naming the container over a body reading `Running sessions move to Detached.` and one row per owned live session as `label (agent)`, answered with `Cancel` (focused), `Remove anyway` (unregister, leaving them running under Detached) or `Stop and remove` (a cleanup-free `StopSession` each, then the removal).

  A workspace's own sessions are those whose `workspace_id` is it; a repo's are those whose first member repo is it, outside any workspace; a plain shell is never listed. A removal is silent and closes the dialog once the daemon's list no longer holds the container, a refusal is the generic "Daemon error" toast with the dialog left open and the arm dropped, and Esc, Enter, Space and Tab drive the dialog without reaching the terminal.

  The menu's first row is `Spawn session`: the spawn dialog held on that container, with the target picker showing it alone and Open in left free.

  When the container has a stored config, `Launch last again ▸` follows: that submenu replaces the rows with `‹ Back`, `Current tab`, `New tab`, one row per grid tab by name, then `Edit first…` (the spawn dialog prefilled and locked to the container instead of a replay).

  Then `Appearance…` and `Launch preset…`, `Launch preset…` lists the container's presets (repo and workspace containers alike): a loading row until the `Presets` reply carrying that menu's own request id arrives, a failed row when the `Error` carrying that id answers instead (an error without the id toasts on its own and leaves the row loading) and a none-defined row; picking a preset opens the preset wizard.

  Below a separator sit `Open in Explorer`, `Open in VS Code` and `Copy path`; the danger Remove row follows below another separator. Explorer and Copy path act on a repo's folder; a workspace has no single folder, so both are disabled with the tooltip `This container has no folder`, as is VS Code when a workspace yields neither a linked file nor a folder.

  Opening the menu checks off the UI thread that each target still exists; the rows stay enabled while it runs, and a gone target disables its row with `This folder no longer exists`, `This workspace file no longer exists` or `None of this workspace's folders exist` (Copy path is never disabled, and a host that opens no local files runs no check). A workspace whose member folders are partly gone opens only the ones that exist, and an open whose target vanished after the check toasts `Couldn't open` with `{path} no longer exists`.

  VS Code opens a repo's folder; a workspace opens its linked `.code-workspace` file when one is set, else its member folders in member order as one multi-root window (`--add` for each after the first), skipping members with no folder. When VS Code is missing, the folder opens in the file manager or the workspace file in its default app, under the `VS Code not found` warning toast.

  A repo or workspace whose stored spawn config has launched before draws a two-line 46 px row.

  The name line sits over a muted last-launch summary (`container-<key>-summary`) of the agent's descriptor label, run mode and target, then each field set away from its default in the descriptor's order (a choice's label, a typed value, a toggle's label), then `trusted`, then `N env`, absent parts omitted (`Claude · Interactive · new worktree from main · Accept edits · opus · 2 env`). The row's tooltip is `Last launch: <summary>`, and a never-launched row stays 30 px.

  A click focuses the container row and folds it; while the sidebar holds keyboard focus the focused row draws the 2 px `ACCENT` ring (`container-<key>-focus`), Up and Down move that focus among the visible container rows, Enter and Space fold, and Esc clears it and returns the keyboard to the terminal.

  The focus also clears on a Repos/Tabs switch, a Repo filter change, a switch of the activity rail away from Sessions, a fold of the sidebar, and when its container leaves the list, so the keys never act on a row that is not drawn.

  A repo's or workspace's row shows a launch ▶ at its right while the pointer is over it or it holds the keyboard focus (`container-<key>-launch`, hover `Launch last again in the current tab`); it replays without folding the row.

  The ▶ goes once the pointer leaves the window (`sidebar::shown_hovered_row`; Windows sends no mouse-exit event, only the window's hover change) and when the sidebar folds or switches away from Sessions; the row's own hover background is GPUI's `.hover()` and can stay until the pointer next moves in the window.

  A double-click on the row replays into the current tab too: its first click still folds or unfolds the row, and its second replays instead of folding again. Two launch gestures for one container within 500 ms count as one, so a double-click or a repeated ▶ launches once.

  With no stored config, both open the spawn dialog locked to the container; a trusted config opens the dialog prefilled, after the `Review trusted launch` warning toast; a worktree config waits for the daemon's fresh `SuggestBranchName`, replaying with that name (a reply is matched by its `request_id`, and a timeout raises `Couldn't pick a branch name for launch-last`), while an in-place config replays at once.

  A stored config that is headless (`SessionMode::Headless`) is never replayed: the row gesture (double-click or ▶) launches nothing, opens no dialog and raises a Warning toast `Headless runs aren't supported in this client` with no detail, and `Launch last again ▸` is dimmed with that text as its tip, its click doing nothing.

  A captured config drops the one-shot answers a launch gave — the worktree-collision policy (back to `Reuse`) and an in-place target's `checkout_strategy` (back to `None`) — so a replay, a duplicate, a recovery or a resume never acts on a collision or dirty tree nobody was shown.

  A tab container's row has its own right-click menu, one row, `Spawn session here`, on a grid tab only; a diff tab's menu has no rows. It fixes Open in to that tab and starts the dialog on the target of the tab's first session, when it has one, leaving the target picker free; if the tab goes while the dialog is open, the lock drops and placement falls back to the current tab, else a new one.

  A DIR or SH container draws a folder banner under its row while expanded (`container-<key>-banner`): the folder path on one truncated line (`container-<key>-path`), then `Add repo` (`container-<key>-add-repo`, hover `Add repo: <folder>`) and, once the folder's scan answers with a suggestion, `Add workspace` (`container-<key>-add-workspace`, hover `Add workspace: <suggested name>`). The links keep their room while the path ends in `…`.

  The banner folds with its container, and only DIR and SH containers draw one: a repo, workspace, tab, Unbound or Detached container never does, nor an SH with no cwd.

  `Add repo` registers that folder straight away (`AddRepo { path, name: None }`), opening no picker and leaving the toolbar's Add repo… start folder (`last_repo_dir`) alone. `Add workspace` accepts the scanned `VscodeWorkspaceSuggestion` as it stands (`AcceptVscodeWorkspaceSuggestion { watch: false }`), with no creator dialog and no `needs 2 repos` gate.

  The client sends `ScanVscodeWorkspaces` once for each distinct DIR or SH folder on a connection, keyed by the folder's normalized path (case-folded on Windows): a folder first seen in a session list or update, or regrouped by a repo or workspace registry change, is scanned then, and the same folder is never rescanned. Answers for a folder are dropped on `Welcome` and while the connection overlay is up, so a reconnect scans again.

  An expanded Detached container shows: `These sessions are still running, but their repo or workspace is no longer registered. Resume one from its row, or stop it from its header.` The toolbar's `⋯` menu gains `Resume all abandoned (N)` under a separator when N ≥ 2, sending `ResumeAllAbandoned`.

  While one of its leaves still runs, the banner also carries a two-click `Stop all` chip (`detached-stop-all`, hover `Stop every session still running in this container`): the first click arms it to `Confirm: stop all` (`Click again to stop every running session`), the second stops every leaf that still runs, and Esc disarms it.

  The chip is absent while every leaf is stopped or the activity rail is on another panel, and an arm is dropped when the container or the sidebar folds, or the rail leaves Sessions.

- **Workspace creator** (`workspace_creator.rs` model, `workspace_creator_view.rs` view): `Add workspace…` follows `Add repo…` in the `⋯` menu; under two repos it draws muted with `needs 2 repos` and does nothing, and the menu's tooltip names only the rows shown. It opens `New workspace`, with `From repos` and `From VS Code workspace` tabs; From repos needs a name and at least two ticked members, and says `No repos registered yet.` when none exist.

  From VS Code workspace parses the picked `.code-workspace` file, showing `Parsing workspace file…` then the suggested name and its folders in file order with `registered` / `will register` badges under the `Workspace name` label; a parse failure shows the daemon's message inline, else `Failed to parse workspace file.`.

  The file picker filters to `.code-workspace` on Windows and opens at `last_vscode_dir`, else `last_repo_dir`, storing the picked file's folder. On a remote host the tab shows the label `Workspace file on {host}` and a path field (`workspace-creator-file-input`) in place of the file line and Browse: Enter sends `ParseVscodeWorkspace` with the typed host path, a refusal shows inline, and From repos says `No repos on {host} yet.` when none exist.

  Create keeps the dialog open until a `Workspaces` broadcast holds the trimmed, case-insensitive name; an `Error` with no `request_id` shows inline with the typed name and ticks intact and raises no toast, while a routed `Error` takes the generic path.

- **VS Code workspace detected** (`vscode_suggestion.rs` model, `vscode_suggestion_view.rs` view): the daemon's suggestions queue and show one at a time. The prompt shows the file path in monospace, `<path> references N folders. Create a rustling-tulip workspace named <name> with these members?` with the name bold, then the folder badges.

  The buttons are `Not now` / `Create` / `Create & watch` (tooltip `watch the file and re-sync members`), dismissed by ✕ (`Dismiss suggestion`) or Esc; `Create & watch` sends the watch flag, and answering shows the next queued prompt.

- **Preset wizard** (`preset_wizard.rs` model, `preset_prompts.rs` prompt parsing, `preset_launch.rs` preview grouping, job registry and toast text, `preset_wizard_view.rs` view): a modal opened from the container menu's `Launch preset…`.

  Its stages walk the preset's prompt source (a prompts file through a filtered file picker, a folder, inline text or GitHub issue ranges, limited to the sources the preset allows) and then its prompted variables (text, folder and file with `Pick…` and `Clear`, case-insensitive toggles).

  The last stage is Preview: `{n} prompt(s) ready to spawn.` (plus ` → {t} tab(s).` when the preset groups by tab), the prompts as `[n] first line` under `Tab i of m` headers, a `Max panes per tab` field (hidden without tab grouping; blank takes the preset's cap, a preset cap of 0 counts as 1; input that is not a whole number of 1 or more disables Launch with an inline message; read-only while scripts run) and a `Scripts to run` section listing each script command with its variables filled in.

  Launch runs the scripts on the daemon first, then sends `LaunchPreset` with a client-made job id and closes the dialog; a script failure stays on Preview with the error inline and the failing script's row marked in the danger colour, and the button reads `Retry script(s) & launch …`. Esc, Cancel and Back wait while the scripts run; a new `Welcome` and the connection overlay still close the wizard.

  Progress outlives the dialog. A job registry in `RootView` tracks only the jobs this window started, so another window's launch raises no toast here.

  Each job has one sticky toast, `Launching {preset}` with `{created} of {total} sessions · {tabs created} of {tabs} tabs`, a thin progress bar (`notice_view.rs`) and a red `Cancel` button (`CancelPresetLaunch`), replaced in place by a `Launched {preset}` or `Cancelled {preset}` toast; a failed job raises one sticky warning, `Preset launch failed: {preset}`, with the daemon's reason. Closing a running job's toast keeps it closed until the job ends.

  Success and canceled toasts stay timed and keep `Select` and a red `Stop all` until they fade; a failed launch's toast is sticky until ×. A job leaves the registry when its terminal toast closes, times out or is pushed out. On `Welcome` running jobs and their toasts drop, and ended ones keep theirs. Toast buttons are generic (`ToastSpec::actions`, selectors `toast-action-<toast id>-cancel|select|stop-all`).

  `Select` tints the job's sessions in the sidebar (palette `LAUNCHED_TINT`) and shows the Sessions view; on a running launch the tint follows the job, so sessions it makes later are tinted as they arrive. A user's session click (sidebar leaf or Needs You row) or `Welcome` clears the tint; a programmatic selection does not.

  It only highlights. `Stop all` stops the job's live created and partial sessions with no confirm and keeps their worktrees; on a running launch it sends `CancelPresetLaunch` first. `Cancel` leaves the toast once pressed. For a tab-grouping launch the first tab the job creates is shown once its id is known.

  A non-optional prompted variable is required and a toggle never is: `Next` stays disabled with the tooltip `Fill in: <labels>` until each is filled. The last stage's `Next` is disabled with the tooltip `Preview and launch arrive with the next update`. Esc, a lost connection or a new `Welcome` closes the wizard.

## Daemon connection and protocol

- The client ensures the daemon through `crates/daemon-client` (`ensure_running`, passing `NATIVE_PROTOCOL_VERSIONS` from `net.rs`). A running daemon is reused when its supported versions intersect the client's, even from another build, so the client never fights another client over a stale binary; a daemon sharing none is retired through `/shutdown` and replaced, and its tracer sessions reattach.
- daemon.json carries `supported_versions` (`#[serde(default)]`; an older file reads as its scalar `protocol_version`).
- The native client speaks only the current protocol version, and the daemon's `supported` lists only that version; there is no TypeScript mirror on `main`.
- **Client identity**: the native client keeps its id in the file `client-id-native` and sends it in `Hello`; the daemon keys its tab layout on it.
- **Reconnect**: backoff 0.5 s → 10 s, a standby-resume watchdog, a connecting overlay with "Restart daemon", and a footer pill with a troubleshooting flyout. After a user-chosen shutdown the client never reconnects or respawns the daemon.
- **Connection target** (`target.rs`): `Target::Local` is the loopback daemon above (`ensure_running`, `ws://127.0.0.1:{port}/ws`, the `daemon.json` token). `Target::Tailnet(TailnetHost)` names a MagicDNS host (lower-cased, trailing dots trimmed, port default 8787) and dials `wss://{name}:{port}/ws` over TLS that trusts the OS certificate store (rustls, config built once per net thread); there is no plaintext or self-signed fallback, and an empty store fails closed.

  A tailnet target never runs `ensure_running` and never sends `Shutdown` or Stop; its `Hello` carries the device token read from the `DeviceTokenStore` (`NetDeps.tokens`), and a host with no stored token, or an unreadable store, is dialled with an empty one. `NetCommand::SwitchTarget` drops the socket and connects at once with fresh connection state.

  `Target::Ssh(SshHost)` names an `~/.ssh/config` alias or `user@host[:port]`, kept exactly as typed; one that is empty, starts with `-` (ssh would read it as an option), holds whitespace or a control character, or has an empty side of its `@` is refused (`Target::parse_ssh`). The client runs `ssh -T -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o ConnectTimeout=10 -o Compression=no -- <host> <remote command>` with no console window (`ssh.rs`), the remote command defaulting to `rustling-tulipd bridge` and a saved host's `remote_command` replacing it, and speaks the protocol as one JSON message per line over its stdio.

  The SSH login is the owner: there is no device token and no pairing, because the bridge on the host fills the `Hello` token. `BatchMode` means ssh never prompts, so an unknown host key or a key needing a passphrase fails with ssh's own text; when ssh's stdout ends the connection closes with `daemon closed the connection: ` and the last 2 KB of ssh's stderr, trimmed. The ssh binary is `RUSTLING_TULIP_SSH` (empty counts as unset), else `ssh` on PATH, else `%SystemRoot%\System32\OpenSSH\ssh.exe`; none found closes with `ssh was not found: install the OpenSSH client or set RUSTLING_TULIP_SSH to its path`.

  An SSH target is a remote host like a tailnet one: no `ensure_running`, no Shutdown or Stop, local-file actions off, Restart reads "Reconnect now", and the footer pill and overlay use the host part of the string. The pairing views never act on it: a `PendingDevices` the daemon pushes on a loopback connection is ignored there (no Waiting devices rows, no toast).

  On a remote host Restart becomes "Reconnect now" (reconnect without touching the host's daemon, and the way out of `AuthFailed` and of `NotApproved`) and Stop is unavailable. The connection model gives the footer pill `{short name} · {state}`, the overlay "Connecting to {short name}…" and a flyout "Host" row `{name}:{port}`; the footer, flyout and overlay render them, and a target change clears the flyout's Port, PID and Protocol rows.

  **Remote mode** (`remote_mode.rs`): views ask a capability table (`Capabilities::for_target`), never the target.

  Local-file actions (Reveal, Open in Explorer or VS Code, the Browse buttons and the preset wizard's Pick… buttons; every picker goes through `RootView::pick_local`) stay visible but disabled with the tooltip "Not available on a remote host: this opens files on this PC", and a refused click toasts "Couldn't open" with that detail; a Ctrl+click on a terminal path link fetches the file instead (see "Fetched files").

  Where a path must name a file or folder on the host, the view takes it typed instead: Add repo, the workspace creator's `.code-workspace` file and the preset wizard's file and folder rows (source and variables, the folder row prefilled with its default) and the Shell… dialog's folder are text fields checked by the same host-path rules (`repo_add.rs`, `check_host_path_rules`: not empty, absolute in Windows or POSIX form, no `..`), and a failing row shows its refusal and holds Next.

  Copy path stays, since it touches no file. Stop, the Handshake file row and Open daemon.log are hidden, because they act on the host's process and files. Closing the window on a remote host quits at once and sends nothing, even if an exit dialog is open.

  **Host path prompt** (`host_path_prompt.rs` model, `host_path_prompt_view.rs` view): Add repo is the one local-file action a remote host keeps, as a typed path. Its five entries (the sidebar note button, the ⋯ menu row, the empty-pane button and the spawn dialog's no-repos button, by click or Enter/Space) open a modal "Add a repo on {host}" instead of the folder picker, which stays the local behaviour; the ⋯ row's tooltip reads "Type the path of a folder on the host to register as a repo".

  The field (placeholder "Full path on {host}", hint "A folder on {host} that holds a git repo.") has Cancel and Add after it: Tab cycles field, Cancel, Add; Enter adds and Esc closes. Add sends `AddRepo { path: the trimmed text, name: its last segment }` and closes, and a daemon refusal arrives as the usual error toast.

  The host may run another OS, so the client checks the text by string rules only (`repo_add.rs`, `RepoAdd::from_host_path`, `HostPathProblem`, `host_workspace_file`): empty, not a full path (a drive letter, `\\server` or `/` start) and a `..` segment are refused inline with the field kept, and the workspace creator's file field also wants a `.code-workspace` name. A typed host path never reads or writes `last_repo_dir` or `last_vscode_dir`.

  The remembered quick-shell folder is this PC's: on a remote host `+ Shell` and an empty pane's Shell here send no folder (the host daemon's default), and the Shell… dialog neither prefills it nor offers to save or clear it.

  Its folder is judged by those host-path rules: the hint reads the Add repo prompt's refusal for that host ("Use a full path on {host}, starting with a drive letter, \\server or /." or "Use a path without .. in it.") and the placeholder reads "A full path on {host}". On this PC the check stays this PC's full-path rule, with the hint "Enter a full path, like C:\Users\you".

  The daemon itself refuses a standalone shell or agent folder that is not a full path by its own OS's rules, answering with "Couldn't start session" / "The folder must be a full path on this host.".

  On a tailnet target `Welcome` is held until the next frame: `Welcome` then `DevicePairFailed` "device not approved" gives `State::NotApproved { reason, had_token }`, the socket is kept, nothing reconnects on close, and only Reconnect now or a target switch redials. The footer reads "not paired" with the error dot, the pill "{host} · not paired", the tooltip "not paired — device not approved" and the first-connect overlay "Pair with {host}".

  **Pair dialog** (`device_pairing.rs` model, `device_pairing_view.rs` view): "Pair with {host}" opens by itself after the owner picks a tailnet host (also the current one while it is not paired) or presses Reconnect now, never at boot; while not paired the footer daemon pill opens it instead of the flyout.

  Intro ("{host} does not know this PC." or "{host} no longer accepts this PC's pairing.") offers Ask to pair and Close. Asking ("Asking {host}…", Cancel) becomes Waiting ("Approve this PC on {host}.", `expires in m:ss`, a keyboard-reachable `Have a code?` link, Cancel) until the owner approves on the host PC.

  `Have a code?` reveals the 6-digit field with Pair and Cancel while the wait continues: Pair sends `DevicePairRedeem`, so a code minted by `rustling-tulipd pair` on the host can be typed there.

  A wrong or expired code shows in the same refusal line as CodeEntry's and the request keeps waiting, while a failure that ends the request (remote access off, too many attempts, device not approved) ends it with its reason.

  An approval meanwhile moves the dialog to CodeEntry with an empty field (a code typed while waiting is cleared, since the approval has its own code). CodeEntry ("Type the code {host} shows.", a 6-digit field, `code expires in m:ss`, a refusal line, Pair, Cancel). Paired closes the dialog: the net thread stores the token and reconnects.

  When the host refuses a request that has gone out (the net thread emits `NetEvent::PairRequestSent` after writing it; a blocked device is refused this way), the dialog ends on "{host} did not accept this PC's request." with Ask again and Close, instead of spinning.

  Denied, expired, superseded, ended, a dropped connection, "Could not save this device's token; pair again." and the host's own reason end on a line with Ask again and Close; a failed redial after Ask again withdraws the request. Esc sends `PairCancel` while a request is out. The typed code never reaches a log. One token store (`RootDeps.tokens`, passed to `net::spawn` and `NetDeps::production`) is shared by the net thread and the view.

  `native.log` records at info "device pairing: the dialog opens" (fields `host`, `had_token`), "… asking the host", "… sending the code", "… withdrawing the request" and "… paired; the dialog closes", and warns "device pairing: the token was lost with no dialog open".

  **Target switch** (`RootView::reset_for_target`): the echoed connection state whose target differs from the current one, handled before the new daemon's Welcome, clears the old daemon's sessions, repos, workspaces, tabs, panes, source-control status, git History, diff tabs, Recover history and badge, worktrees root and branch suggestions.

  It closes every dialog and menu a reconnect's Welcome closes (Settings included), plus the Recover dialog, the exit dialog, the pane-close and Move panes dialogs and the empty-pane menu, and ends any pane, pill, sidebar, divider or split drag. It sends nothing to either daemon and writes nothing to `native-ui.json`, and `native.log` records `connection target changed: clearing the old daemon's state` at info.

  **Host picker** (`connection_picker.rs` model, plain Rust; `connection_picker_view.rs` view): the footer's `Connections` button, left of the daemon pill on every target, and a `Connections` link beside `Reconnect now` in the first-connect overlay open a centred modal drawn above that overlay, and no key reaches a layer under it.

  The modal is one 440 px panel kept 24 px inside the window, its list scrolling, in three sections: "This PC", "Saved hosts" (with its count; hidden when empty) and "On your tailnet" (its note reads "N online", "Looking for your devices…" or the scan error; hidden once a scan offers no unsaved peer). Every host is one two-line row with a kind icon (`assets/icons/host-pc.svg`, `host-tailnet.svg`, `host-ssh.svg`): the name (a tailnet suffix greyed) over a second line ("This PC's daemon · 7 sessions", "Tailnet · port 8787 · 3 sessions last time", "SSH · rustling-tulipd bridge", "Online · port 8787 · connecting saves it"), and a state chip. The current host shows its live session count without "last time".

  The chip reads "Connected", "Connecting…", "Needs pairing", "Not paired" or "Disconnected". A saved tailnet host with no stored device token is "Not paired"; a saved SSH host is never "Not paired" (tokens are read once, when the picker opens; an unreadable store warns "reading the host's device token for the picker: …").

  Clicking a row switches to it. Clicking the current host's row sends nothing, and any other click sends `SwitchTarget`, closes the picker and saves `native-ui.json`: `last_target`, and the clicked or typed host into `saved_hosts` with the port it was dialled on (a known host's saved port is updated), so connecting a tailnet peer saves it. The header X is mouse-only. Esc closes an open add form first, then the picker.

  One `+ Add host` opens an inline form with a Tailnet | SSH switch. Tailnet has Name and Port (prefilled 8787) fields with Cancel and `Add and connect`, and a refusal shows under them and sends nothing. Each mode keeps its own typed fields when the switch moves away; Cancel and Esc drop the typed text.

  Forget sits on a saved row and shows on hover or when Tab reaches Forget itself (focus on the row keeps the chip); while hidden it still takes a click. It rewrites `saved_hosts`; for a tailnet host it also deletes the host's device token (a failure warns "deleting the forgotten host's device token: …" and toasts "Couldn't forget this host's device token"), and for an SSH host it deletes no token and drops the `ssh:<name>` session count. Forgetting the current host switches to This PC and clears `last_target`. An SSH host and a tailnet host of one name are listed and forgotten separately.

  The SSH mode of the add form has Host (placeholder `devbox or user@host`, an alias from the SSH config or `user@host`) and Remote command (placeholder `rustling-tulipd bridge`) fields, the test result line and the buttons Cancel, `Test connection` and `Add and connect`. Refusals show under the fields and send nothing: "Type an SSH host: an alias from your SSH config, or user@host.", "An SSH host cannot start with a dash.", "An SSH host cannot contain spaces or control characters."

  `Test connection` runs ssh as a connection would, sends a `Hello` with an empty token and no client id or name (so it keys no tab layout), and shows "Testing the connection…", then `Connected: <short name>`, ssh's stderr verbatim, the daemon's `AuthFailed` reason, "ssh ended without an answer." (or "…: <error>"), or "No answer from the host in 20 s." with the stderr so far. The 20 s guard sits on top of ssh's own 10 s connect timeout, and the button reads `Cancel` while the test runs, killing the ssh child. Enter in an SSH field runs the test; editing a field cancels a running test and clears its result, and so does switching to Tailnet mode.

  `Add and connect` needs no passing test: it saves `{"kind":"ssh","name":…,"remote_command":…}` with no port, replacing any record of that name whole, and switches to it. Switching to an SSH host never asks for pairing.

  The model: the rows are This PC, then the saved hosts in saved order, then the owner's peers from `tailscale status` that are online, untagged and not shared in (peers pass the DNS-name check and dial port 8787; a peer that is also saved is listed once, with the saved port).

  A typed host is checked as the name first, then a port of 1 to 65535, and each refusal's `Display` is the message shown. A failed scan keeps the saved rows and shows `TailnetError`'s `Display` text. The scan runs on a one-shot thread with its own current-thread runtime behind the `PeerSource` seam, so neither the UI nor the net thread waits on it. Forgetting the host the client is on reports that, so the caller switches to This PC.

  **Boot target**: `native-ui.json` keeps `saved_hosts` and `last_target` (absent or null for this PC), each entry with a `kind` (`"tailnet"` when missing, or `"ssh"`): `{name, port}` for a tailnet host, `{kind, name, remote_command}` with no port for an SSH host. The client reads them before it starts the net thread and boots on `last_target` when a saved host has its kind and name, dialled as it was saved, and never runs `ensure_running` for it; a saved host that cannot be reached is still the boot target.

  The per-host session count for an SSH host is keyed `ssh:<host string>`.
  A `last_target` that is not a saved host boots on this PC and logs at info `the last target is not a saved host; starting on this PC`. A hand-edited `saved_hosts` entry that fails the DNS-name check or has a port outside 1 to 65535 is dropped on load with a warn, its name normalised otherwise, and a `last_target` that is not a host record loads as this PC.

  While the target is a remote host, `native-ui.json`'s `active_tab_id` is not updated, and per-tab font sizes and folds and the source-control pins and collapse state are not pruned, so the Local values stay in memory and on disk; a font size, fold or pin set by hand on a remote host still saves. Switching back to Local shows the saved active tab again.

  On a remote host the empty-registry notes name the host (`No repos or workspaces on {host} yet.` in the sidebar, `No repos on {host} yet.` in the spawn dialog), and Reconnect now in the flyout or the overlay redials the same host.
- **Request ids**: a client that must match a reply to its request sets the optional `request_id` (spawn, duplicate, scrollback, appearance, git reads, spawn previews, checkout prompts, session history, recovery, env names, branch-name suggestions, and the tab requests `CreateTab` / `MergeTabs` / `MovePane` / `ExtractToNewTab` / `SplitPane`), and the daemon echoes it only to the requester (CLAUDE.md, "Wire-protocol gotchas").

  A tab request's id comes back only on the refusing `Error`, which a refused split also logs at warn as `tab layout request failed`.

  A new tab, merge or move-to-new-tab arms a pending create (an ordered queue of request ids; the oldest is consumed when a new tab arrives, which becomes active); the refusing `Error` disarms its id and a `Welcome` forgets every armed one, so a refused request never makes the next arriving tab active.

  The client also keeps tab-layout requests in flight per tab (a split; a move, for its source and destination tab): the tab's next `TabUpdated` ends the oldest entry, `TabRemoved` ends all of them, the refusing `Error` ends its own, and `Welcome` forgets every one; closing a tab's last pane while one is in flight does not mark the tab closing. Adding one is additive; a reply without one (an older daemon) falls back to matching by content.

  The spawn dialog's `SuggestBranchName` asks without one, so every suggestion for its target still reaches it, while a launch-last wait takes only the reply carrying its own id.

- Unknown daemon message types are logged with their tag and skipped; a frame that fails to decode is logged as an error.
- The client logs to `logs/native.log`, rotated on each launch.
- On Windows a build without debug assertions (release, test-release) is a GUI-subsystem exe, so an installed launch opens no console window; a debug build keeps the console and its stderr log.
## Persistence

Machine-local UI state lives in native-ui.json in the config dir (`UiState` in `sidebar.rs`; the field list is in CLAUDE.md, "Where things live on disk"). Tab layouts, session appearance overrides and host settings such as the keep-awake mode and the worktrees root live on the daemon, so every client sees them. Saves are debounced (layout and tab font sizes 500 ms) and flushed on quit.

Settings → Worktrees edits the daemon's worktrees-root override, and shows `Saved` only once the daemon has echoed the last save. Manage worktrees (opened from that tab) lists the root's groups titled by their `.rt-group` name, deletes one group or every stale one, and "Launch session here" opens the spawn dialog with the target locked and that worktree pinned (the spawn form's `Lock`, which a duplicate prefill reuses).

On a remote host the path field stays enabled with the hint "Created on {host} if it does not exist." and Browse stays disabled. Folder and file pickers go through `RootDeps.pick_folder` / `RootDeps.pick_file`, so specs inject fakes: GPUI's test platform cannot prompt.

## Tab layouts

A client's tabs and their split trees are daemon-side and keyed by client id (`state.rs` `ClientLayout`), so two clients can show the same sessions in different tabs and layouts. A tab's layout type is a `RearrangeLayout`; `TabEntry.picked_layout` records the type last picked for that tab, and holds only while the user has not arranged the tab's panes by hand since.

A tab's content is a grid, a diff, a viewer or the Dashboard. A viewer tab (`TabContent::Viewer { path }`) stores only the path of the file it shows, in the connected daemon's layout for the client id; it has no panes, so `TabEntry::grid()` and `grid_mut()` return `None` and every pane operation refuses it ("not a grid tab").

`OpenViewerTab { id, path }` opens or focuses by exact string match on the path, as `OpenDiffTab` does: a new path creates the tab, named after the path's basename (splitting `/` and `\`, else the whole path), and broadcasts `TabUpdated`; a path already open only focuses its tab and broadcasts nothing.

The reply is `ViewerTabOpened { id, tab_id }`, and an empty path is refused with an `Error` carrying the open's id. The daemon compares the path as sent, so a client trims and normalises it first.

Viewer tabs survive a `state.json` reload and are copied verbatim when a client clones another client's layout. The path is the client's own local path: on a remote host it puts a local path in the host's `state.json`, which the daemon never reads. The client draws the tab's body as the text viewer (see "Text viewer").

A client has at most one Dashboard tab (`TabContent::Dashboard`, a non-grid tab like a viewer). The activity rail's Dashboard button sends `OpenDashboardTab` only when no Dashboard tab is listed and otherwise activates the listed one; the daemon answers `DashboardTabOpened`, names the tab "Dashboard" and adds it last. `RestoreTab` and `RestoreTabSnapshot` skip a Dashboard while another is listed (logged at info), and Undo of a closed Dashboard while one is open focuses the open one. The tab body draws the By status view (`dashboard_view.rs`): a header strip, a queue strip, a sticky column header and the Needs you, Working, Idle and Done groups of two-line session rows, with nested agent-tree rows under Working rows (design and wording rules in `docs/plans/dashboard.md`). The client keeps a facts store (`dashboard_facts.rs`) that merges `DashboardFacts` deltas, is pruned to the listed sessions, is cleared on `Welcome`, a target switch and unwatch, and drops facts that arrive while unwatched. The rules and number formats live in `dashboard_rows.rs` as plain Rust with unit tests, and `dashboard_view.rs` only renders what it returns. The view repaints every second while any age is under a minute, else every 30 s.

The client sends `WatchDashboard { on }` as the Dashboard tab becomes or stops being the active tab, sends on again after a new `Welcome`, and resets its watch state without sending on a target switch. The tab pill shows a dashboard icon and the need-you count, with no busy pill; the rail button carries the Needs You count as its badge and is lit while the Dashboard tab is active.

Dashboard and viewer tabs cannot be renamed (no Rename row in the tab menu, and a double-click on the pill starts nothing), and a Dashboard tab cannot be popped out (a saved pop-out key naming one is not opened).

A pick marks the tab with `RearrangeTab`; an `ExtractToNewTab` that names a layout marks its new tab too (the first-connect chooser's overflow tabs, the Move-panes dialog; the chooser's first tab is rearranged). The remembered type is the one the daemon actually built, so a wire layout it does not know is kept as `Balanced`.

While a tab is untouched since its pick, the daemon rebuilds it on every pane addition or removal: `SplitPane`, `ClosePane`, a session-removal close across each client's tabs (`close_session_panes`), the source of `ExtractToNewTab`, and both sides of a cross-tab `MovePane` that does not set `touch`. A rebuild keeps pane ids, their order and unbound panes.

A concrete grid keeps its column count (a three-column pick with a fourth pane is three plus one) and carries `aspect: None`.

The automatic pick names no count at all: the tab menu's `Auto` row (the Grid submenu's, and the flat row a two-pane tab shows), the first-connect chooser's `Auto` shape, the Move panes dialog's `Auto` shape and Ctrl+Shift+G all send `RearrangeLayout::Grid { cols: 0, aspect: Some(<pane area width / height>) }`, on an additive `#[serde(default)]` field and without a protocol bump.

The daemon keeps the tab automatic and fits the columns to that aspect with the aspect-aware best fit (`crates/daemon/src/tabs.rs::best_fit_grid_cols`, moved there from the client), falling back to `ceil(sqrt(N))` when no aspect is known: an older client's bare `cols: 0`, the `AutoGrid` preset and the all-sessions `InitLayout` build. Every later rebuild re-picks from the aspect the tab remembers, so adding or removing a pane keeps the grid fitting its area.

While a tab stays automatic, a window resize re-sends its automatic layout with the pane area's new aspect after a 250 ms debounce (`apps/native/src/lib.rs::schedule_auto_grid_resend`, `AUTO_GRID_RESIZE_DEBOUNCE`; `apps/native/src/tabs.rs::auto_grid_resends` skips a tab whose stored aspect is within 0.01 of the new one), so a drag settles as one re-pick.

The daemon treats a repeat automatic rearrange of an already-automatic tab as a re-fit: it rebuilds over every pane the tab holds, keeping the empty (unbound) panes and the tab's untouched mark and adopting the new aspect, where a first pick and any fixed pick still drop the unbound panes as a tidy-up. A tab pinned to a fixed count keeps it, and a resize never touches it.

`SetPaneRatio` (a divider drag), any same-tab `MovePane`, a swap included, a cross-tab `MovePane` with `touch` set (it clears the destination's flag; the source tab, which lost a pane, is still rebuilt) and a `SwapPanes` (it clears both tabs') clear the flag, after which panes keep the sizes they have. `MergeTabs`, `CreateTab` and `InitLayout`'s own build leave it unset, a preset launch clears it, and undo snapshots and cloned layouts carry it as is.

The client sends no `SetPaneRatio` for a divider press that stays within 3 logical px of where it landed, so a click on the handle does not count as a resize. A tab update that rebuilds the dragged tab's split tree renumbers its splits, so the client ends the in-flight drag instead of sending its held path.

**Import remote sessions** (`layout_chooser.rs` model, `layout_chooser_view.rs` view, beside the first-connect chooser): the client records each remote host's session count in `native-ui.json` as `host_session_counts` (`{"<lower-cased MagicDNS name>": <count>}`), only while the host is in `saved_hosts`; Forget drops the entry. It is written on the host's first session list after `Welcome`, on every later full `Sessions` list (a resync) and, debounced, after a session is added or removed.

A host with no recorded count records silently. On a reconnect whose first list is non-empty and differs from the recorded count, and unless the first-connect chooser is due, the modal "Import remote sessions" opens: "The remote has N session(s) (d more/fewer than before)." and "Opening them or starting empty replaces this window's tabs on this host. Sessions keep running."

Its rows are Keep existing layout (`import-keep`), Open all N session(s) (`import-open-all`) and Start empty (`import-empty`), with a header ✕ (`import-close`). Esc and ✕ keep, and focus starts on Keep. Open all sends `InitLayout { AllSessions }` and arranges the reply's first tab as the chooser does, through its shape and per-tab picker; Start empty sends `InitLayout { Empty }`. A dialog that closes over the modal (cleanup-failed, a modal notice) returns the keyboard to it.

## Drag model

`apps/native/src/drag.rs` is the plain-Rust half of every drag. `DragPayload` is a `Pane`, a `Tab`, a `Container` or a `Leaf`. `drop_edge` maps the pointer's fraction of a pane to a `PaneDropEdge`: the outer band (`OUTER_BAND`, 0.12) on a side that touches the grid boundary (`tabs::outer_edges`, within 0.5 px), the nearer outer edge at a corner; the centre box (`CENTRE_LOW` 0.35 to `CENTRE_HIGH` 0.65) is `Replace`; otherwise the half on the nearer axis.

`pane_drop` builds a `PaneDrop` with its undo label: `undo::SWAPPED_PANES` for a centre drop onto a pane that shows a session, in the same tab or another, `undo::MOVED_PANE` otherwise. Every edge or centre drop on a pane sets `MovePane.touch`, so the daemon counts the destination as arranged by hand; a pill drop and the pane menu's Move to ▸ tab do not (the daemon rebuilds both tabs).

A cross-tab centre drop onto a bound pane sends `SwapPanes` (one atomic request: the two panes exchange sessions, both tabs keep topology and pane ids and are touched, each broadcasts a `TabUpdated`), and onto an empty pane sends `MovePane` `Replace` with `touch`.

A swap focuses the destination pane, never marks its source tab closing, and is in flight on both tabs until a reply or an update settles it. Undo restores each tab's snapshot, focusing the pane that holds the session the tab had. `pane_drop` sends nothing for a diff or missing tab, a pane a grid does not hold, a self-drop, a non-pane payload, or a same-tab drop onto the facing half of the source's own split sibling, which would rebuild the same tree.

A pane drag starts from the header's drag area: the title, the glyph and the empty stretch, not the buttons, chips, Stop or exit code, whose press goes to `press_on_header_control`. `drag::pane_target` finds the pane under the pointer and its edge from the pane rects, so the shown tab's grid carries one drop layer over it (`grid_view.rs`): it does not occlude the panes, and it stops mouse propagation only while a drag runs, so terminals get no mouse reports then.

A refused drop (what `pane_drop` sends nothing for) shows no overlay and sends nothing; an accepted one goes out through `send_pane_move`, the path the pane menu's Move to ▸ shares.

The overlay (`drag::drop_overlay`) is the accent at 22% over the half with a 1 px border, a strip 14% deep at 40% with a 2 px border for an outer edge, or the centre inset 12% on every side with a `Swap panes` badge. The ghost is a pill of the header's label (`Empty pane` for a pane with no session) on `GROUND` at 78% with the pane's accent border.

Esc cancels the drag whichever element has focus (the root's key capture), and a tab update that arrives mid-drag ends it when it touches the drag's source tab or the shown tab. A pane drag over a grid pill activates its tab, so a drop lands in the shown tab, which may not be the tab the drag started in.

`reorder` moves one id before or after another (`Side` from the pointer's half of the row or pill); `reorder_tabs` and `reorder_containers` build `ReorderTabs` / `ReorderContainers` and give none when the order would not change.

A pill reorder applies locally at once and carries a `request_id`; the client remembers each pending reorder's previous order. The daemon echoes the id only on the `Error` that refuses it. A refusal of the only pending reorder snaps the strip back to its order before the drop (tabs created since keep their place after); with newer reorders pending, a refused one hands its remembered order to the next and leaves the strip.

A `TabsReordered` echo settles only the reorder whose sent order it matches, and does not move the strip while a newer reorder is pending. A fresh tab list or a reconnect forgets pending reorders. The toast is the generic daemon error.

The sidebar drags use the same payloads. In the Repos view a workspace or repo container header drags as a `Container`, and a drop sends `ReorderContainers` with every shown workspace and repo in the new order; in the Tabs view a tab container header drags with the strip's `Tab` payload, so a pill and a tab container drop on each other and send `ReorderTabs` (a refusal puts the strip back, as above).

A leaf of a workspace, repo or tab container reorders inside its own container and sends `ReorderSessions` with that container's whole shown order, which the Tabs view also applies from the stored session order. SH, DIR, Detached and Unbound rows neither drag nor take drops, their leaves drag (below) but never reorder (`drag::leaf_reorders` is true only inside workspace, repo and tab containers), and a leaf dropped on a container header does nothing.

A leaf drags to the grid and the tab pills whatever its container. Over the grid it shows the pane-drop overlay (`drag::leaf_drop`, sent by `grid_view.rs` `send_leaf_drop`). A bound leaf moves its pane: the active tab's pane showing the session, else its first binding in tab order (bindings in a tab being closed do not count), and records `undo::MOVED_PANE`, or `undo::SWAPPED_PANES` for a swap.

An unbound leaf is placed with `SplitPane` on a half edge, where an outer edge acts as the same-side half of the hovered pane and the overlay shows that half, or `ReplacePaneSession` on the centre or on any edge of an empty pane; it records no undo entry, as placing a leaf by click does not. A headless or unknown session drops nowhere.

On a tab pill (`drag::leaf_pill_drop`, `tab_bar.rs` `on_pill_drop`) an unbound leaf goes where `pane_target_for_session` puts it in that tab, and a bound leaf's pane moves to that spot (`MovePane` with `touch: false`, "Moved pane"), or nothing happens when the tab already shows the session.

This differs on purpose from a pane dropped on a pill, which uses `pick_balanced_drop_target`: a leaf carries a session, not a pane position, so it follows the placement a click would give. A diff, missing or closing tab refuses the drop and shows the no-drop cursor (`pill_no_drop`). Hovering a pill with a leaf does not switch to that tab, unlike a pane drag.

The pointer's half of the target group picks the side: an insertion line, a 2 px accent line, draws at its top or bottom, and the dragged group fades to 0.4 with a ghost of its label. The new order shows at once and holds until the daemon's broadcast; a refused reorder shows the error toast and the local order stays until the next broadcast or reconnect. A container may be dropped on a collapsed container, which does not auto-expand. Esc cancels.

Under a non-manual Sort repos the Repos view shows the sorted order, while the stored order stays the manual one. A drop onto a row in another sort group is refused and draws no line (`SidebarModel::same_sort_group`).

Inside a group the line follows the displayed order and the drop moves the dragged container next to the target in the manual order (`RootView::sidebar_drop_message`). Under Manual a drag is unchanged, and with the Active filter on it reorders the full manual list, so hidden repos keep their place.

## Settings

The Settings modal's tab list reads General, Notifications, Spawn defaults, Worktrees, Remote access, Appearance and App title, then a non-selectable `Agents` group row and one tab per agent the daemon lists, in the daemon's order, each labelled with the descriptor's label (selector `settings-tab-{tag}`). The group row is a label only: no page, click, hover or keyboard stop. With no agents listed (disconnected) there is no `Agents` row and no agent tab.

Remote access (Tailscale) is absent from the list while the client's target is a remote host, and Settings falls back to General.

It lets the user's other devices on their tailnet connect to this PC's sessions. A switch `Allow remote access over Tailscale` mirrors the daemon's last `RemoteStatus` and sends `ConfigureRemote` on toggle.

Beside it are a status line per `RemoteState`, `Address: https://{name}:{port}` and the certificate's expiry while serving, a Port field with Apply (1-65535, inline error), and hints that changing the port or the PC's Tailscale name changes the iPhone app's address and that the daemon keeps running while remote access is on.

The tab also lists the tailnet devices asking to connect, so the owner approves a new device on the host PC (the device gets its token only then). A `Waiting devices · {n}` section shows one row per request (name, Tailscale login, a `m:ss left` countdown, Approve… and Deny; an approved row reads `Approved · waiting for the device` with only Deny), and the tab carries an amber count badge (`99+` cap, approved requests included).

Approve… opens an approval dialog that sends nothing until Approve is pressed, then shows the QR code, the code as `735 160` and `expires in m:ss · this request only`, and closes by itself when the code runs out; disconnecting or switching target closes it and forgets the waiting requests.

A `Devices` section (`devices.rs` model, `devices_view.rs` view) follows Waiting devices, shown only on this PC's daemon when its `Welcome` lists `device_block`. It lists the approved devices (`ListDevices` on `Welcome` and on selecting Remote access, folded from `Devices`), one row each with name, login and a status: "Connected", "Connected (N)", "Last seen just now / Nm ago / Nh ago / Nd ago", "Never connected", or the "Blocked" tag.

Rows sort connected first, then by last seen (newest first, never-seen last), then blocked, ties by name; an empty list reads "No device has paired yet." An unblocked row has Block and a blocked one Forget (`ForgetDevice`). Block opens a confirm, "Block {name}?" with "Its live connections end now, and it cannot pair again until you forget it.", Cancel (focused first) and Block (danger); Tab and Shift+Tab move, Enter presses, Esc cancels, and Block sends `RevokeDevice`.

A request is asked for only on a connect to this PC's daemon whose `Welcome` lists `device_pairing`, never on a tailnet or SSH host. Each new unapproved request raises a toast, "A device asks for remote access" with the body `{name} · signed in to Tailscale as {login}`, with Approve… and Deny, only while connected to this PC's daemon. Pairing toasts do not consult the Notifications tab's toggles.

Approve… on a toast brings the window forward and opens the dialog without opening Settings, and only when no pairing dialog is open and the request is still listed and unapproved: an open dialog is never replaced from a toast.

Deny sends `DenyDevice` for a listed request, and a press on the toast body only brings the window forward. `native.log` records a press as "pairing: a toast was pressed" (field `action`), and one that arrives after its window closed as the warning "a pairing toast was pressed after its window closed".

A button pressed from Action Center reaches the running client through the toast activator (see Notifications): Approve… is a foreground activation (the window comes forward, the Confirm dialog opens), Deny is a background activation. A press while the client is closed starts it with `-ToastActivated -Embedding` (logged "started by a toast press"; the flags are never read as a session id).

Approve and Deny presses that arrive before the pending-devices list are held (`ToastActionQueue`) and applied when it arrives; a press for a request no longer listed is dropped with a log line, and held presses are dropped with a log line on a remote target, a daemon without device pairing, or a host switch.

`Start the daemon when I log in` registers the HKCU Run value (a LaunchAgent on macOS) through `daemon_client::autostart`, behind the `Autostart` seam in `remote_access.rs` (`RootDeps.autostart`). The controls are inert until the first `RemoteStatus` arrives, and a new connection forgets it.

Spawn defaults holds "Trusted launch by default" (hint: "When enabled, new agent sessions skip their approval prompts."), the only Settings value saved in `native-ui.json`'s `spawn`.

An agent page's first section, titled with the descriptor's label (Claude's reads "Claude"), draws one row per descriptor field marked `settings: true`, in descriptor order, with the descriptor's choice labels (Claude: "Permission mode", then "Provider").

A Toggle is a checkbox row, a Choice a segmented row, and a Text or Model field an input that applies on Enter, on Tab out, on blur and when Settings closes, only when edited; a field of a kind this client does not know shows the note "Update the client to change this". The model is not a Settings field for Claude; it is chosen per spawn. A text field's placeholder follows the page's values.

Those rows read the connected daemon's `AgentSettings` for the page's agent and change it with `SetAgentSettings` (the current map merged with the changed key), so the setting follows the connected daemon, not this machine. A row changes only when the daemon's broadcast arrives; two quick changes are merged so neither is lost. While disconnected or before this connection's agent settings arrive, the page shows no option rows.

A change logs `settings: setting an agent option` (fields `agent` and `key`) in `native.log`; a press whose control no longer matches its field logs at debug `settings: pressed control does not match its field; ignored`, and a text apply with no value logs `settings: the text field takes no value now; nothing sent`.

Selectors are `settings-{tag}-{field}-{choice}` for a choice (the field id's `_` becomes `-`: `settings-claude-permission-mode-{cli-default|default|accept-edits|bypass-permissions|plan}`, `settings-claude-provider-{anthropic|deepseek}`), `settings-{tag}-{field}` for a toggle, a text input and the unknown-kind note, and `settings-{tag}-{field}-locked` for the locked note.

A page whose agent a connected daemon no longer lists gives way to General and its drafts are dropped; a dropped connection keeps the page and its drafts until the next `Welcome`, which closes Settings.

While trusted launch is on, only Claude's Permission mode row locks: it is disabled, shows the stored value and carries the note "Ignored while trusted launch is on. The chosen value is kept for when you turn trusted launch off."; Provider stays editable. The launch rows below stay reachable in the Tab ring.

Each agent page ends with a "Launch command" section with a Program field (placeholder the agent's tag) and an Extra arguments field. Drafts and pending applies are per agent, and `SetAgentCommand` names the page's agent; an apply logs `settings: applying an agent's launch command` (field `agent`). Enter in either sends both, trimmed, as `SetAgentCommand`; both empty clears the command.

There is no Apply button. `Saved` shows after the daemon's echo, only while both fields still match what was stored. Notes say the command is used for new sessions, Restart and Resume and not for headless runs, and that extra arguments follow POSIX shell rules (quote a Windows path or double its backslashes; in cmd a doskey macro is not expanded, so set Program to a `.cmd` file).

While an apply waits, the daemon's launch-command refusal shows under Program (when its text starts "The launch command's Program") or under Extra arguments (the other refusals) instead of a toast. It clears on the next edit and shows again on return if another Settings tab was open. Any other daemon error during an apply ends it and still toasts.

With the agent's override variable set on the host (`RUSTLING_TULIP_CLAUDE` for Claude; the name comes from the daemon's `program_env`), Program is read-only, out of the Tab ring, and says "Set by RUSTLING_TULIP_CLAUDE". A daemon without the `agent_commands` feature shows both rows dimmed with the tooltip "Restart the daemon to set the launch command: .\rt.ps1 restart". Before `AgentCommands` arrives, or while disconnected, the section is not drawn.

Drafts follow another client's `AgentCommands` only while untouched; text typed after Enter survives the echo, and re-selecting an agent's tab keeps its drafts and a pending apply. Selectors: `settings-agent-program`, `settings-agent-args`, `settings-agent-saved`, `settings-agent-program-error`, `settings-agent-args-error`, `settings-agent-program-env`.

## Terminal

- **Scrollback-first attach**: `LoadScrollback` with live output buffered meanwhile; an 8 s request timeout with retries after 2 s and 4 s, each with its own `request_id`, and a reply to an older id dropped. When the daemon restarts the forwarder (`forwarder_restarted: true`) the held-back output is discarded, since it is already in the history. The logical scrollback cap is 5000 lines.
- **Size and replies**: only one pane per session (its size driver) answers terminal queries, so a session shown in two panes gets each reply once.

  Across clients, the daemon lets only the client with recent input own a session's size. Every `Resize` carries `claim`, true when the pane had input in the last 5 s (typed keys, IME text, paste, Re-run command, mouse presses, releases that end a gesture, wheel and pointer moves to a new pixel) or when this client just resized the pane.

  Such a resize is the client's window resized or maximized, a split divider dragged (one claimed `Resize` per painted size), the sidebar dragged, folded or unfolded (Ctrl+B or the activity rail; a rail click that only switches panels claims nothing), or a font-size change made on this client.

  Each such action marks the panes it resizes, and only the next measure of a marked pane claims. The first input after 5 s idle re-sends the pane's size with `claim: true`, ahead of the input.

  An attach, a pane re-bound to another session, a reconnect, a tab switch or showing a hidden tab, a focus change, and another client's layout or appearance change send `claim: false`, which the daemon ignores for a session a client has already sized. A pane that does not own the size draws as before, with no indicator, and when the owner disconnects the next client to claim wins.

  ConPTY repaints the whole screen on every resize, even one to the size the PTY already has, so the daemon skips a claimed `Resize` that matches the size it last applied. A test that parses the PTY stream must replay it through a terminal grid rather than strip escapes.

  A query that ends the loaded history is answered unless live output follows it, which keeps a fresh ConPTY shell from sitting blank.
- **Copy and paste**: Ctrl+C copies with a selection, else sends ^C; Ctrl+Shift+C copies; copy on select is a saved setting (default on); paste on Ctrl+V and Ctrl+Shift+V, bracketed when the child asks. Shift+drag selects even under mouse reporting. OSC 52 `c` stores reach the system clipboard; reads are answered empty and stores in replayed history are dropped.

  A selection that crosses a box-drawn TUI panel (such as Claude Code's) copies only its text, via `strip_box` in `copy_text.rs` (called from `Term::selection_text`). The panel's borders, rules and padding are left out, the text keeps its relative indent, and the top rule's title is kept as the first line.

  A `├─┤` divider becomes one blank line, and text is never joined across it. Paragraphs the panel wrapped are rejoined with a space, measured in terminal columns against the panel's width minus its right padding, except rows that start with a box-drawing character, a line number or a diff marker, hold an inner bar, or end in `;`, `{` or `}`.

  Box-drawn tables, ASCII `+--+` art, a literal `│` in a sentence and a selection with no panel copy exactly as selected. A triple-click (Lines) copy keeps its trailing newline, tabs copy as `\t`, and OSC 52 stores are unchanged.
- **Keys and text input**: Shift+Enter sends the bytes the session agent's descriptor names in `shift_enter` (`lf` sends LF; `backslash_cr`, an unknown value, a plain shell and an agent with no descriptor send `\` + CR). GPUI gives Space no `key_char`, so `keys.rs` maps Space and Shift+Space itself; Alt+Space stays unmapped so Windows opens the system menu.

  Text arrives through an `EntityInputHandler` on the pane, so dead keys, AltGr and IME composition work: a composition is drawn underlined at the cursor even when the program hides it, and dropped on blur or stop; a dead key is not reported to GPUI as a composition, because GPUI would then hide Enter, Backspace, Esc and arrows from the pane. Ctrl+Alt with a non-alphanumeric character types it (AltGr on layouts GPUI doesn't know); Ctrl+Alt+letter or digit sends ESC+char.
- **Links**: `links.rs` ports the Tauri detector (URLs, absolute, UNC and relative paths with `:line:col`, stitched across soft and hard wraps and TUI borders within ±64 rows). Links underline only while Ctrl is held; Ctrl+click wins over mouse reporting.

  A path to a viewable text file opens in a viewer tab (see "Text viewer") while **Open text files in the external app** is off; otherwise (a file that is not viewable text, or the setting on) a path with `:line` opens in VS Code with `-g` and a plain path in the OS default handler.

  A path Windows flags as dangerous, or with a script or executable extension, asks "Run <name>?" first. A UNC path opens only for a host behind a mapped drive or listed in `unc_hosts` in native-ui.json, which the client reads on each click and never writes.
- **Shell integration**: OSC 133 and 633 marks drive per-command gutter dots (ok / fail / unknown) anchored to absolute rows through scroll, reflow and trim, with a menu to copy the command, the output or both, and to re-run (typed without Enter).
- **Prompt jumps** (`prompt_nav.rs`): two header buttons on Interactive and PlainShell panes, between "Move this pane to a new tab" and close, `↑` ("Previous prompt", `pane-prompt-prev-<pane>`) and `↓` ("Next prompt", `pane-prompt-next-<pane>`). Previous scrolls the nearest sent prompt above the top row to the top; Next scrolls to the next prompt, or from the newest to the live bottom.

  A prompt is a Claude pane's `❯ ` row (slash commands included; the live prompt box and `❯ N.` dialog options excluded) or a plain shell's OSC 133 prompt mark; the client scans its own grid, so the daemon is not involved. The buttons scroll the terminal directly (nothing reaches the child, even under mouse reporting) and close an open shell-dot menu. With nowhere to go a button is drawn at 45% opacity, never hidden, so the buttons never move; there is no keyboard shortcut.
- **Theme and fonts**: every UI colour is a token in `palette.rs` (the Contrast look chosen in `docs/design/ui-language/README.md`: a near-black ground, floating layers well above it, session rows as cards, ruled menu groups; contrast floors pinned by its tests, every text step at WCAG AA or better, and quiet text lifted to secondary on a hovered row). The terminal uses a fixed palette with ANSI colours contrast-adjusted against the background (`theme.rs`), on a built-in ground of #070609; the built-in session accent is Coral #F07A62, with six accent presets (Coral, Rose, Ochre, Periwinkle, Sky, Lilac); a program's OSC 11 background and OSC 12 caret colour win.

  The interface draws in Geist by default (`fonts::DEFAULT_UI_FAMILY`); `fonts::ui_family(cx)` is the current choice, set on the root view and the tooltip view, and the terminal font pickers never list Geist, Inter or Schibsted Grotesk.

  Settings → Appearance has a "UI font" row under "Shell background": a filter field and a list (Use built-in, Bundled: Geist, Inter, Schibsted Grotesk, System: installed fonts) with a hint `Resolved: <family> from app|built-in`.

  A pick applies to every window at once (`fonts::set_ui_family`, `RootView::set_ui_font`) and is saved in `native-ui.json` as `ui_font_family` (null or absent is Geist); a stored family that is no longer installed falls back to Geist with a log line (`fonts::resolve_ui_family`).

  Geist Mono, Fira Code, JetBrains Mono and Cascadia Code are bundled for terminals, and Geist, Inter and Schibsted Grotesk at 400 to 700 for the interface (OFL, `assets/fonts/`); a family resolves against the installed list, falling back to Geist Mono, Cascadia Mono, Consolas, then Courier New.
- **Status glyphs**: a session's status draws as a shape as well as a colour (`status_glyph.rs`, tinted SVG layers in `assets/icons/status-*.svg`) in sidebar leaves, pane headers, Needs You rows, the tab bar's busy pill and the footer.

  It is a spinning arc while working, an amber diamond with `?` while asking, a faint hollow ring while spawning, a small dot when idle or stopped (dimmed), a red dot with `×` on error, and a ring with a spinning arc in the Working colour while the turn is over but background work still runs (the snapshot's `background` flag on an `Idle` session).

  An agent session (never a plain shell) whose turn ends (working to idle) while this client is not showing it focused is an **unseen turn** and shows a green hollow ring until its leaf is clicked or a focused pane shows it; any other status clears it. The unseen set is per window and not saved.
- **Petal chrome**: the look follows the user's Petal canvas, https://claude.ai/artifact/B5hxrVTyyR635CQecEpG2N, saved in `docs/plans/canvases/petal/` (a board per view; the Dashboard, Settings → Alerts, Conversation and phone boards are targets for the waves that build those features).

  The rail is 52 px on `SUNKEN` with 36 px tiles and a 2 px coral active bar; the tab bar 38 px on `SURFACE`, the active tab on `GROUND`; the footer 28 px on `SUNKEN`. Panes are cards with an 8 px radius, 6 px apart inside 6 px of padding (`PANE_GUTTER`; the sidebar and source-control handles keep `DIVIDER_WIDTH`), the terminal inset by the 1 px border with square inner corners.

  A pane header is one 36 px row (`RAISED` when focused, else `SURFACE`, a 3 px accent line on its left edge) holding the title, then the runtime chip, one filled chip per entry of the snapshot's `agent_chips`, `repo:branch`, `+N` (further workspace members, listed with their worktree paths in its tooltip), and a trusted chip, then 26 px buttons.

  A Claude session routed to DeepSeek shows a `DeepSeek` agent chip (element `pane-chip-<pane>-provider`; the text and the hover tip, "Runs on DeepSeek", come from the daemon), and a plain shell or an Anthropic session shows none. When it runs short, the `repo:branch` chip truncates first, then the title, and the buttons never move.

  Dialogs are one card (`SURFACE`, 1 px `LINE_STRONG`, 14 px radius, a 16 px bold title, a raised footer) over a 66% scrim; menus, popovers and toasts sit on `CHIP` with a 10 px radius. Buttons are outlined, coral primary or danger-outlined (never filled) in `buttons.rs`'s sizes; disabled is 45% opacity, and keyboard focus is a 2 px `ACCENT` ring outside the control (a sidebar container row draws its ring only while the sidebar holds the keyboard).
- **Truncating text with `…`**: gpui 0.2.2's text element reuses its cached shaping whenever a measure call carries no wrap width (`elements/text.rs`), and taffy's first measure of a flex child never carries a definite one, so `.truncate()` and nowrap + `text_ellipsis` clip mid-letter without an `…`, and wrap + `text_ellipsis` + `line_clamp(1)` can vanish outright in a stacked split, whose panes get a 0-width pass first.

  No view calls `.truncate()`; one-line text goes through `ellipsis.rs`, which has two shapes. `ellipsized(id, text, drawn)` is for a content-sized box that shrinks (the pane title, chips, sublines, the diff path): an invisible copy sizes the box to the whole text, and an absolutely positioned copy, laid out once at the box's final width, draws the `…`.

  `truncating(id, text)` (wrap on, `text_ellipsis`, `line_clamp(1)`) is for a `flex_1` or block row outside the pane grid (panel rows, heads, the footer), where the width is definite once flex settles; a site whose spec shows `truncating` drawing whole text in a narrow box uses the pair instead (the source-control picker went the other way: the pair drew whole there). Text that runs past its box by about a pixel now ends in `…` where the old clip hid the sliver.

  The source-control section head is two lines, repo then branch, each cut on its own. The in-process harness's fake text layout hides the cause, so specs read the drawn text: in debug builds each helper records its `TextLayout` under its id once a test calls `ellipsis::enable_probe()`, and `ellipsis::drawn_text(id)` returns it; `tests/ui/ui_ellipsis.rs` holds one spec per site.
- **Session menu** (a leaf, a pane header or a Needs You row, right-clicked): the state actions (Stop, Restart, Resume, …) first. Then, behind a separator, come `Duplicate ▸`, then `Move to ▸` when a pane shows the session or `Add to current tab` / `Add to new tab` when none does (one `Add to new tab` row when the placement would open a tab anyway, and neither when the session can't attach), then `Reveal worktree` for a session with its own worktree (it opens `members[0].worktree_path` itself).

  Then, behind a separator, come `Appearance…`, `Accent ▸` and the checkable `Don't count as busy` (offered in every session state; the ✓ sits in the 12 px leading slot; it sends an absolute `SetSessionBusyExcluded` and keeps no optimistic state).

  A submenu swaps the rows for `‹ Back`, `New tab` and the grid tabs. Duplicate ▸ keeps the original, places the copy by pane target and brings that tab forward with the copy focused; a headless session's row, and its Restart, are dimmed with the tip `Headless runs aren't supported in this client`.

  Move to ▸ lists grid tabs not already showing the session, moves the right-clicked pane when opened from a pane header (else the first one showing it), and a move into a tab can be undone; Restart and Resume rerun the session in its own worktree and branch, continuing a Claude session's conversation while its transcript exists, then replace the original's panes and discard it (keeping the worktree).

  A Resume of an abandoned session keeps the old session's name and accent, and the new session takes the old one's place in the sidebar.

  Shift on a Duplicate ▸ choice asks the daemon for the source's stored spawn config (`GetSpawnConfig`) and opens the spawn dialog on its reply, locked on the source's repo or workspace, prefilled after the Spawn defaults so the source's runtime, run mode, trusted flag, its own agent's options, model, env rows, base and worktree toggle win, with Open in set to the clicked target.

  The branch follows the daemon's duplicate rule (a worktree source gets a fresh suggestion, an in-place one keeps its branch), Advanced opens only for settings the source set, and a prefilled worktree toggle never saves the repo's worktree default.

  With no stored config the dialog opens on the source's repo with the defaults; a standalone source duplicates as a plain click does, since the dialog has no bare-folder target; a dialog that can't open when the reply arrives says why ("Duplicate failed"). The pending request is a slot keyed by session id (`session_actions.rs` `Prefills`). The model is `session_actions.rs`, the view `session_menu.rs`.
- **Spawn dialog agent fields** (`spawn_form.rs`, `spawn_view.rs`, `agent_options.rs`, `agent_options_view.rs`): the Runtime row is the daemon's agents from `DaemonMessage::Agents`, each labelled by its descriptor's `tag`, then `Plain shell`; with none listed, or before `Agents` arrives, only `Plain shell`. Main fields draw under Runtime in descriptor order, and Advanced fields sit above the env rows in descriptor order. A plain shell shows no agent fields.

  Choice fields draw segmented or as a dropdown by their style, toggles are checkboxes, and text and model fields are inputs (a model field adds suggestion chips). A field of unknown kind is a disabled row reading `Update the client to change this`. Help text shows as a note under the field.

  A Choice whose default is no value gets a first choice `CLI default`, and a Text or Model field with no default shows the placeholder `CLI default`. Selectors are `spawn-opt-<field>-<choice>` (`spawn-opt-permission-mode-plan`, `spawn-opt-provider-deepseek`, `spawn-opt-permission-mode-cli-default`), `spawn-opt-<field>` for a toggle or input, and `spawn-opt-model-sonnet` for a chip.

  A field locked on a trusted launch carries the note `Ignored while trusted launch is on. The chosen value is kept for when you turn trusted launch off.` and is disabled, showing its kept value (a text or model one is read-only).

  The Trusted launch checkbox's detail line is the agent's `trust_note` (Claude: `Trusted launch passes --dangerously-skip-permissions.`), and none shows for an agent without one. The warning reads `Approval prompts are bypassed for this session.` The worktree checkbox note reads `Unchecked: run the agent in the repo's main directory`.

  A field's initial value is the first that exists of: the agent's Settings value (`DaemonMessage::AgentSettings`), the target's last spawn of that agent, a Shift-duplicate source's value, then the user's edits. After a target switch the fields the user set keep their values and every other field follows the new target's last spawn.

  Values are kept per agent across Runtime switches. Duplicating a plain shell changes no agent's values, and a source naming an agent the daemon does not list falls back to the first listed agent with its Settings values.

  Advanced opens on its own when an Advanced field starts off its default (from Settings, the last spawn or a duplicate source) or env rows are prefilled, and keeps following target switches until the user opens or closes it. On the wire `SpawnRequest.agent.options` is sparse (only values off the field default), a trusted launch omits locked fields, and a plain shell sends `agent: None`.
- **Spawn dialog env rows** (`spawn_form.rs`, `spawn_view.rs`): each row has a Secret toggle, ticked while the key matches `protocol::env_rows::is_secret_key` until the user touches it; a ticked row's key goes in `SpawnRequest.secret_env_keys`.

  A ticked row shows the note "Saved in Windows Credential Manager; only a reference is kept." and masks its value while typed (`TextInput::set_masked`: dots drawn in place of the text, copy disabled), with a Show / Hide button. An un-ticked row whose key still matches the pattern shows "Still saved securely: the name looks like a secret.", since the daemon seals it anyway.

  A prefilled `${secret:<id>}` value renders as a read-only "Saved secret" with a Replace button that clears it for typing, and the row keeps sending the reference until replaced. Typing `$` in a value field, or Down, offers the variable names `ListEnvNames` returns, each labelled with its scope; a pick writes `${env:NAME}`, and a reference row has no Secret toggle. The daemon side: [env-secrets.md](./env-secrets.md).
- **No headless runs** (`spawn_form.rs`, `grid_view.rs`, `session_actions.rs`): the spawn dialog has no Mode row or Prompt field (Tab goes Branch, Random, Base branch, Trusted launch) and the client sends only `SpawnRequest.mode` Interactive or PlainShell, with `initial_prompt` null.

  A headless session that exists anyway (spawned by another client) keeps its sidebar leaf and an inert tab pill; a pane bound to one shows the muted line `Headless session — not shown in this client` (no stats bar or action log) and keeps it once stopped, with no exited overlay; the note holds the pane's focus, so a click focuses the pane and app shortcuts such as Ctrl+N still work. The daemon's headless path and the protocol are unchanged.
- **Session labels, chips and endings**: every label goes through `display_label`, with the full label in a tooltip. A session's terminal title names it once the agent's title glyph is stripped (the descriptor's title rules), and a title that is only the agent's placeholder falls through to the session label, then the runtime tag, then the id.

  The tooltip's "Terminal title:" line shows the stripped title and is left out when the title falls through. Member chips get a second pane-header row only for sessions with members. The orphan banner names the session's own runtime. A worktree-less session that exits on its own is discarded, except headless sessions (still kept on exit) and abandoned ones. The rename field keeps the native seed.
- **Agent exited into its shell** (`notices.rs`, `notice_view.rs`, `branch_fate.rs`): on `DaemonMessage::AgentExited` the client raises an Info toast `{Label} exited. The pane is a shell now.` (the session agent's descriptor label, so `Claude exited. …` for Claude; `The agent exited. The pane is a shell now.` when the session has no agent or its descriptor is gone).

  The toast has no detail, is timed by `TOAST_LIFETIME` (8 s, no countdown), is keyed `agent-exit-<session id>` so a second exit for the session updates it in place, and has one non-destructive button `Close session`.

  An exit for a session the client does not list raises nothing. `Close session` sends `StopSession { cleanup: [] }` unless the session is already `Stopped` or `Error`, then `DiscardSession { cleanup: [] }` (worktrees stay on disk), and the toast goes; ignoring the toast sends nothing and leaves the shell.
- **Runtime tag**: the sidebar leaf's tag and the pane header's runtime chip share `runtime_label`: the session's `program_name` when the daemon reports a non-empty one, else the descriptor's `tag`, else the agent id.

  The tag wears the agent's accent only when the descriptor's `accent` is Claude and the text is the tag itself, so a shell's label on a Claude session shows a plain tag. An interactive Claude session reads the shell (`pwsh`, whatever `RUSTLING_TULIP_SHELL` resolves to) once Claude exits and `claude` again on its next start; a plain shell with no known program shows no tag.
- **Leaf density**: sidebar leaves draw Comfortable (the default: a label line over a subline holding the branch, a workspace's `repo:branch · repo:branch`, or a shell's folder with home as `~`, the tab pills at its end or on a chip row below when they don't fit) or Compact (one 26 px line, the subline in the tooltip), set in Settings → General → Sidebar and saved as `general.leaf_density`.
- **Sessions under repos**: the row after Leaf density in Settings → General → Sidebar (`settings-sessions-under-repos-every|busy|none`), saved as `general.sessions_under_repos` (`"every"`, `"busy"`, `"none"`; a missing key or an unknown value loads as `"every"`, the default). Every session draws every leaf under a repo or workspace row.

  Busy only draws just the leaves that are Spawning, Working, AwaitingInput (Needs you) or Error, or carry background work, an unseen finish or the attention mark. None draws no leaves. In both thinned modes the leaf of the session the focused pane shows is always drawn, and the row keeps its session count (the total) and its fold chevron. Shell, folder and Detached rows, `containers()` and everything reading it (the Needs-you rail, hover, drag, the folder scan) are unchanged.
- **Sort repos**: the row after Sessions under repos (`settings-repo-sort-manual|active|needs-you`), saved as `general.repo_sort` (`"manual"`, `"active"`, `"needs_you"`; a missing key or an unknown value loads as `"needs_you"`, the default). The repos view sorts repos and workspaces together.

  Needs you on top puts those with a session in the Needs You list first (`needs_you::is_listed`: never a parked or abandoned session), then those with a live session, then the rest. Active first puts active ones, then the rest. Each group keeps the manual order, and shell, folder and Detached rows stay last. Manual keeps the manual order.

  A repo is active when a live session (not Stopped or Error, not abandoned, not parked; orphans, headless runs and plain shells count) has a member worktree path, `worktree_paths` or `current_cwd` at or under the repo's path, or members naming the repo.

  A workspace is active by its `workspace_id` or an active member (`sidebar::active_container_keys`). The order follows status changes as they come and keyboard row focus follows the displayed order. The sort is display-only (`SidebarModel::display_containers`): `containers()` and everything reading it keep the manual order.
- **Active filter**: a segmented control "All" · "Active" in the Repos view's sidebar header, left of "Repos" · "Tabs" and absent in Tabs (`sidebar-filter-all`, `sidebar-filter-active`), saved as the top-level `repo_filter` of `native-ui.json` (`"all"`, `"active"`; a missing key or an unknown value loads as `"all"`, the default).

  With Active on, a repo or workspace row shows only when it is active (`sidebar::active_container_keys`, the rule Sort repos uses) or when one of its leaves is in the Needs You list; shell, folder and Detached rows stay. It is a display step in `SidebarModel::display_containers`, so `containers()` and everything reading it keep every row.

  With Active on, once the daemon's repos have arrived and the registry holds at least one repo or workspace, an empty filtered list shows "No active repos or workspaces" (`sidebar-filter-empty`) above any shell, folder and Detached rows (`SidebarModel::active_filter_empty`). Toggling either header control clears the keyboard row focus.
- **Open text files in the external app**: a checkbox row (`settings-general-open-text-externally`) in a Settings → General "Files" section between "Sidebar" and "Terminal behavior", off by default and saved as `general.open_text_externally`. On, a text file a link or a fetch opens goes to the external app (VS Code at the line, or the OS default app) rather than the built-in viewer. The open path reads it (`viewer_text::decide`), so on, a link or fetch opens as it did before the viewer existed.

  In both densities a leaf carries the same filled chips as its pane header, one per `agent_chips` entry, after the tab pills and before the state tag (element `leaf-provider-<session id>`, `-<n>` for the later ones), counted in Comfortable's fit measurement; hovering a chip shows its tip. A leaf that needs you shows the asking glyph and the `!` badge, with no row wash.

  The footer counts sessions by glyph ("N need you", "N working", "N background", "N waiting", a zero count hidden; `footer.rs`), and a tab's count pill and the window title follow the App title page's "Tab and title count" (`title.count`, `tabs.rs` `TabCounts`): by default the number of the tab's sessions awaiting input, parked ones left out, in amber and hidden at zero; "Busy / total" brings back "busy/total" with the working glyph, grey with no glyph at zero; "Idle / total" counts the sessions idle with no background work, grey. A background session counts as busy, is never listed in Needs You, and its leaf goes unseen only once the background work ends.
- **Busy exclusion**: a session marked "Don't count as busy" (the daemon's `busy_excluded` flag) leaves the busy count of the window title and tab pill in the "Busy / total" mode but stays in the total, and leaves the footer's "working" count; the footer's "need you" still counts it when it waits on the owner. The leaf's `!` attention badge and the repo, workspace and tab-container `!` roll-ups skip it, while Needs You still lists it (asking and ended) and OS notifications and spoken alerts still fire.

  The leaf shows a 12 px muted crossed-out pulse in the `!` slot, tooltip "Not counted as busy" (`tabs.rs` `tab_session_counts`, `sidebar.rs` `shows_attention`). Restart and Resume keep the flag (`DuplicateSession.carry_busy_excluded: true`); Duplicate starts the copy counted. The `unseen` mark and the status glyph are unchanged.
- **Appearance levels**: accent, shell background, font family, size and bold resolve session → repo/workspace → app → built-in, each shown with its source. Font size also has a per-tab override (Ctrl+Shift+= / −) above the session level (Ctrl+= / −); Ctrl+0 clears both.

## Source control and diffs

- Status, badge counts and diffs are keyed by (repo, worktree path), so worktree and main-tree counts never overwrite each other. Stashes are one list per repo.
- Git reads carry a `request_id`, so a failed read shows in its own section rather than as a toast; a write whose status refresh fails is answered with `GitWriteError`, so pending state always clears.
- **Diff engine**: `similar` computes line and inline word diffs on the client from the full texts `FileSnapshot` sends (Patience under a 2 s deadline; word spans skipped for a side over 1000 bytes). The daemon refuses a side that is binary or over 2 MiB, with the reason in `FileSnapshot.unavailable`.
- **Diff view**: a custom GPUI element, side-by-side only, rendering aligned rows in one `uniform_list` (synced scroll for free; hunk navigation is `scroll_to_item`; F7 / Shift+F7), with a 10 px overview ruler at its right edge marking each hunk (Ochre mixed, `WAITING` insert-only, `DIFF_DELETE` delete-only). Measured in release on 5k lines and 500 hunks: model build 2.6 ms, first drawn rows 58 ms, worst scroll draw 7.2 ms.
- **Diff tab header**: the file name first, whole, then its folder dimmed; the folder truncates with `…` before the name does, and the full path is in the tooltip. Below 900 px of header width the Whitespace and Highlight toggles show icons instead of labels; the change buttons are 24 px icon squares.
- **Syntax highlighting**: syntect 5.3 plus two-face 0.5 with fancy-regex (no C dependency), six token classes in Petal's diff colours (keywords Rose, functions Periwinkle, types and numbers Ochre, comments `SUBTLE`, strings `LILAC`; `TokenClass::color` in `syntax.rs`), independent of the terminal's ANSI palette and each at least 4.5:1 on the diff ground.

  It runs on the background executor after the uncoloured diff shows, stops past 20,000 lines, a line over 10,000 bytes or 2 s, and is a saved toggle. Cost measured in release: +1.29 MB on the exe (+5.6%), 570 ms for 5k lines of Rust. PowerShell is not coloured: two-face drops that grammar without onig.

  pulldown-cmark and the Markdown view add 573,952 bytes to the release exe (31,178,752 before the Markdown core, 31,752,704 with the view: +1.84%).

## Notifications

OS toasts are built as XML (`toast_activation.rs`) and shown through the `windows` crate's `ToastNotificationManager`. When the client runs as the installed copy (the installer's `uninstall.exe` beside the exe), it registers its own app id `leftos.RustlingTulip` and a COM toast activator at startup and raises every toast (Needs you, Finished, pairing) under that id, so they show as rustling-tulip.

A dev build, a build run from `target\` or a failed registration keeps the PowerShell app id and in-process handlers. The registration writes `%APPDATA%\Microsoft\Windows\Start Menu\Programs\rustling-tulip.lnk` (pointing at the running exe, carrying the app id and the activator CLSID), writes `HKCU\Software\Classes\CLSID\{354BE9DC-6D82-4269-8883-D13CE3E38648}\LocalServer32` (`"<exe>" -ToastActivated`) and sets the process's explicit app id; `native.log` records "toast activator registered".

Two toasts exist. `Needs you` is raised when the daemon's `Attention { AwaitingInput }` arrives (a question, a permission prompt or a plan approval); its body is `<label>: <prompt line>`, the line the Needs You panel shows, or `<label>: Waiting for input`. `Finished` is raised when a live `SessionUpdated` leaves an agent `Idle` with no background work over a copy the client held as `Working`, or as `Idle` with background work; plain shells, a session seen for the first time, an `AwaitingInput` to `Idle` change and a relist or reconnect raise nothing.

A Finished toast for an interactive session that names an agent is held up to 12 s (`FINISHED_HOLD`, `apps/native/src/notify.rs`) while the last `SummarizerStatus` says summaries are on, under the cap and with a usable key; its `WaitingAlert` then titles it `<label> needs your answer`, `<label> finished` or `<label> posted an update` with the alert's notification as the body. An alert that came before the turn counted as finished (background work still running) is kept until the session works again and words the toast at once. No alert in time, a template alert, or no status gives `Finished` with the label as before; the hold is dropped when the session leaves Idle, is removed, or a `Sessions` resync or `Welcome` arrives, and toggles and the skip rule are judged when the toast is raised.

Stopped and errored sessions raise no toast: their `Stopped` and `Error` attentions still drive the sidebar `!` badge and the Needs You panel's Ended rows. A stop the owner asked for sends no attention and records no exit code, so it never reaches Needs You; its pane header reads "stopped" and its overlay "Session stopped". A session that exits on its own keeps its exit code and its Ended row.

The Notifications tab has a `Needs you` toggle, a `Finished` toggle right after it and a `Skip notifications when` row: `Session on screen` (the default), `Window focused` or `Never`. Session on screen skips when the client window is focused and a pane of the active tab shows the session; an active diff tab, or any dialog, modal or pane menu open over the panes, means nothing is on screen.

Window focused skips whenever the window is focused, and Never never skips, for both toasts. Each window decides for itself, and a session marked "Don't count as busy" still notifies.

The settings are `notifications.awaiting_input`, `notifications.finished` (on unless turned off) and `notifications.skip_when` (`on_screen`, `focused` or `never`) in `native-ui.json`. An unknown or non-string `skip_when` loads as `on_screen` with a warn line in `native.log`, and the rest of the file still loads. The tab also shows Windows' read-only toast state with a link to Windows' settings.

## Session recovery

A "Recover sessions" button sits on the activity rail above the Settings gear, with a badge counting unexpected ends (`TracerLost`) not yet recovered (a session a reboot, a crash or a killed process tree ended gets its entry at the next daemon start, so it counts too); an entry counts until it is recovered, from the dialog or the sidebar's Resume, or pruned after 7 days, and Dismiss does not clear it.

The client asks for the history (`ListSessionHistory`) on every `Welcome` and keeps the daemon's broadcasts.

The dialog (`recover.rs` model, `recover_view.rs` view) lists the last 7 days newest first: losses at an unknown time, then bursts of losses within 60 s of each other ("7 sessions lost at 11:29"), then the other ends collapsed behind "Show N other recent sessions" (shown expanded when they are the dialog's only rows; a refresh keeps a toggle the user or a failed recovery made).

Unrecovered losses are pre-ticked; a row already recovered, or with no conversation to resume, is disabled with its reason. Each row offers its conversation candidates (the known id, else the newest) and, for plain-shell rows and rows with no spawn config, a "Recover as" choice limited to what the folder allows.

The choices read the entry's agent descriptor: `{label} session`, `{label} session in {repo}` and `Register {path} as a repo, then {label} session`, the shell choice, and "Plain shell" always.

The shell choice shows as the descriptor's `texts.shell_resume_label` (Claude: "Shell running claude --resume") only when the daemon flags the entry (`SessionHistoryItem.shell_resume`: the plugin has the `shell_resume` cap and the entry has a conversation to resume). A row with nothing to resume shows `texts.no_conversation`.

With no descriptor the label is the agent id ("Agent" for a shell entry), the shell choice reads "Shell resume" and the note "no conversation found". A DeepSeek row offers only Claude options ("DeepSeek sessions recover as Claude."), a row that would start a fresh run is not pre-ticked, and one with no spawn settings is disabled with "no spawn settings recorded".

Sessions still in the sidebar's Abandoned group are listed too, and recovering one hands its panes, its name, its look and its sidebar place to the new session (the daemon rebinds and carries them), as the sidebar's Resume does; a per-session claim on the daemon stops the two paths recovering the same session twice.

The rest of a run's recovered sessions are placed once every one of their snapshots has arrived, in one pass so no two share a pane, where a normal spawn would go; an answer that comes after the 120 s timeout or after the dialog closed is still placed. After a partial failure only the failed rows stay, re-ticked, with their messages.

Sessions a reboot, a crash or a killed process tree ended come back on their own: after the daemon binds, it resumes each abandoned Claude session that qualifies in its old panes (see `docs/architecture.md`), so the client simply sees them running again under their old names, looks and sidebar places, and their history entries count as recovered.

When the daemon's resume pass ends, the client shows one toast keyed `resume-report` (from `ResumeReport`, once per client id per daemon run): Info "Resumed N session(s) after restart", with the detail "M stay(s) in Recover" when M > 0, or Warning "No sessions resumed after restart" when nothing resumed but some were left.

The rest (plain shells, no conversation or transcript found, worktrees that no longer pin, a failed spawn, an in-place checkout on another branch, a worktree another session holds) stay abandoned and wait in Recover and the sidebar's Resume.

## Needs You

A rail item after Sessions (an inbox tray) opens a panel listing every session waiting on the user, headed "NEEDS YOU · N"; its badge counts the rows (hidden at 0, `99+` above 99), and the rail never switches to it on its own.

The list is plain Rust (`needs_you.rs`) over the sidebar model, drawn by `needs_you_view.rs`. A session is listed as **Asking** while its status is `AwaitingInput`, and as **Ended** while it is `Error` or `Stopped` and its leaf is still flagged; parked and abandoned sessions never are.

Rows run longest wait first by the snapshot's `status_since` (the daemon's stamp of the last status change, kept across a reattach), rows with no stamp next, ended rows last, ties in sidebar order.

A row shows the status glyph, the container name in its accent, the session's label and the time waited.

Line two says what an Asking session wants, from the prompt its hooks reported (`pending_input`): a question's header and text ("Migration: keep newest, merge oldest, or skip?", with "+N more" for several), "Allow <tool>: <what it runs>", "Approve plan: " and the plan's first line, or the notification's message, on one line with an ellipsis and in full in the tooltip; with no prompt it reads "Waiting for input" (with the terminal title, minus the agent's title glyph, when it has one).

An Ended row reads "Error", "Exited with code N" or "Stopped".

A click is a leaf click: it focuses or places the session's pane and clears the flag, so an Ended row leaves while an Asking row stays until the question is answered; right-click opens the session menu.

## Text viewer

A text file a terminal path link or the Fetch file… dialog opens goes to a viewer tab (`viewer_tab.rs` the model, `viewer_tab_view.rs` the view, `viewer_text.rs` the plain-Rust core) unless **Open text files in the external app** is on or the file is not viewable.

`viewer_text::decide` probes the file on the background executor and picks the viewer or the external route: a file over 2 MiB, a NUL in the first 8000 bytes, or bytes past those 8000 that are not valid text (UTF-8 or BOM'd UTF-16 via `encoding_rs`) go external. A file over 2 MiB opened from a link or fetch shows the Info toast "Too large for the viewer, opened externally".

- The tab is read-only: a header (file name whole, folder dimmed) with Open in default app, Reveal, Copy path and Copy all (disabled until the file has loaded), a line-number gutter, and a virtualised list of rows 1.4 × the font size high. Syntax colours are always on and a very long line is clamped. A `:line` target scrolls to that line.
- A `.md` or `.markdown` tab opens rendered (`viewer_md_view.rs`): headings, styled paragraphs, nested lists with markers and task boxes, quotes, code blocks coloured in the terminal mono font, tables, rules, raw HTML as muted mono text, images as their alt text, and inline code as a background chip in the body font. The view is a virtual list with a row per code line, list item and table row, so only visible rows are built.
- A click on a link in the rendered view follows it (`RootView::follow_md_link`): an http or https link opens in the browser, and a file link opens that file through the viewer routing (another viewer tab, or the external app), resolved against the Markdown file's folder with any `#fragment` ignored. Any other scheme and a network path raise the Error toast "Couldn't open" with "Only http and https links open from a Markdown file."
- A `#slug` link scrolls the tab so that heading's row is at the top, a heading inside a list or quote included, and an unknown slug does nothing.
- On a tab showing a copy fetched from a remote host, a file link opens by the fetched route and only files under the download folder open: a link resolving outside it raises "Couldn't open" with "Only files in the folder for fetched files open here.", and nothing opens or is sent to the host. Siblings are never fetched, so a missing one gives "No file found for {path}". A tab whose file is not available on this PC follows no link.
- Its header has a Rendered | Source toggle, shown once the file's text has loaded. A click sets that tab's mode and saves `viewer_markdown_rendered` (default true) in native-ui.json; other open tabs keep their mode and new Markdown tabs open in the saved mode. A `path.md:<line>` link opens that tab as Source at the line without changing the saved default.
- A Markdown file over 512 KiB opens as Source with both toggle buttons disabled ("Too large to render as Markdown").
- The Source view (any non-Markdown file, or Markdown in Source mode) selects whole lines (`LineSelection` in `viewer_tab.rs`, handlers in `viewer_tab_view.rs`).

  A left press on a row's gutter number or text selects that line and gives the keyboard to the viewer, Shift+click extends from the anchor, and dragging with the left button extends to the row under the pointer, or to the edge row in view once the pointer is past the list's top or bottom (no auto-scroll); a left release anywhere ends the drag.

  Selected rows are tinted in the text input's selection colour. Ctrl+A selects every line and Esc clears. Ctrl+C copies the selected lines (`ViewerTab::selected_lines`) without gutter numbers, joined with `\n` and with no trailing newline whatever the file's line endings, and shows the usual "copied" chip; with nothing selected it copies nothing.

  Rendered Markdown has no selection, and switching to Rendered clears it. A re-read clamps the selection to the new line count, and one that lands as a banner clears it.
- Find (`viewer_find.rs` the plain-Rust matching and `FindState`, the bar in `viewer_tab_view.rs`) works in the Source view of a tab with text. Ctrl+F opens a one-row bar under the header, or gives an open bar the keyboard again: a query field (placeholder "Find"), a count ("n of m", "0 of 0" with no hit, blank with an empty query) and Previous, Next and Match case buttons.

  Matching is case-insensitive until Match case is on, and updates as you type. The current match starts at the first one at or after the top visible line and is scrolled to the centre. Enter or F3 goes to the next match, Shift+Enter or Shift+F3 to the previous, both wrapping. Every match gets a background tint and the current one a stronger accent tint.
- Esc in the find field closes the bar and returns the keyboard to the tab; Esc in the body clears a selection, else closes the bar. Ctrl+F on a rendered Markdown tab switches that tab to Source and opens find without changing the saved `viewer_markdown_rendered`; on a tab with no text (a banner) it does nothing.
- A file that cannot be shown draws a banner instead of text: too large, binary, not decodable, missing, or not available on a remote host (a restored viewer tab reads nothing there).
- Making a viewer tab the active tab (not a window focus) re-reads its file in place when the file's modified time or size differs from the last read, after one stat on the background executor. Unread, loading and not-available tabs are skipped. There is no hashing, so a same-size rewrite inside one mtime tick is missed until the next change or a re-open.
- A reload keeps the scroll, and rendered Markdown keeps its top row, including any scrolling done while the reload was reading. The selection is clamped to the new line count, find re-runs with its query kept, and the `:line` target is not applied again.
- A reload that finds the file gone or unreadable shows "The file is gone or cannot be read. Showing the last read." over the last lines (their colours and rendered document still finish if they were still being worked out); a debug log line, "the viewer could not stat the file", is written when the stat fails.

  A reload that finds the file too large, binary or undecodable shows that state's banner instead of the lines, and the file is not read again on later activations until its modified time or size changes.
- Opening or re-fetching a file whose viewer tab is already open re-reads it. Open in default app and Reveal take the same route as a link (`open_by` with the tab's recorded `Origin`), so the remote gate and the fetched-copy check apply, and a script is viewed but still asks "Run <name>?" before it runs.

### Design decisions

- **A viewer is a daemon-stored tab kind** (`TabContent::Viewer`), not a client-only tab or a modal: it reuses the pill, activation, close, restore and undo that diff tabs already have, where a client-only kind would touch every `tabs()` call site and a modal would block the terminals.
- **Open text files in the external app on means the behaviour from before the viewer**: VS Code at the line for a `:line` link, the OS default app for a plain path. It names no editor choice, so a later editor setting changes no stored data.
- **A file is viewable text when it is at most 2 MiB (the diff engine's limit), has no NUL in its first 8000 bytes, and is UTF-8 or BOM'd UTF-16.** The sniff decides, not an extension allowlist: the extension only picks the syntax language and the Markdown mode. Anything else goes external, which is no worse than before; there is no lossy fallback decoding.
- **Markdown opens rendered, with a persisted Source toggle**, so a construct the renderer draws wrongly is one click from the source; files over 512 KiB open as Source to bound the cost. The parser is `pulldown-cmark` with default features off (pure Rust, an event model that maps straight onto blocks). The viewer never fetches an image or any other network resource, so images show their alt text.
- **Selection is line-granular**: GPUI has no selectable-text element, and whole lines cover copying a block while Copy all covers the rest. Character selection is not offered.
- **Executable and script files are viewed, not run**: the viewer is safer than handing them to the OS, and its Open in default app button still asks "Run <name>?" first.
- **The extension to language map lives in `viewer_text.rs`** because the daemon's `language_for_path` (`git_inspect.rs`) is out of the client's reach; the test `every_language_id_has_a_grammar` keeps every id it returns backed by a grammar in `syntax.rs`.
- **A reload checks modified time and size on tab activation** rather than watching the file, so an idle viewer costs nothing and a fetched copy, which changes only when fetched again, needs no watcher.

## Fetched files

- Files fetched from a remote host are saved under the `remote-files` root (`daemon_client::remote_files_dir()`), as `<host MagicDNS name, lower-cased>/<repo-name>/<rel-path>` (`ssh-<host string, lower-cased>` instead of the name for an SSH host, so the two kinds never share a folder); names Windows cannot hold are sanitised, and `..`, drive prefixes and leading separators are refused. Nothing under the root is pruned.
- A fetch streams into a hidden `.fetch-<launch stamp>-<n>.part` beside the target, created with create-new and deleted only by the fetch that created it, and is renamed over the target on done. Fewer bytes than announced is an error; more is saved. An error, a cancel, a failed write or a lost or switched connection deletes the part and toasts `Couldn't fetch <file>`.
- One fetch per target: a second fetch to the same path (case-insensitive on Windows) toasts `Already fetching <file>`.
- A Ctrl+click on a terminal path link on a remote host sends `FetchFile` for the link's readings (`fetch_link.rs`, `RootView::fetch_path_link`). A **reading** is one way to take the link's path as a file under a repo or worktree of the pane's session: each candidate, longest first; an absolute host path is matched against the session's member worktrees, a relative one is read against the pane's cwd first, then against each member worktree.

  `.` and `..` are resolved as text, and a path outside every root gives no reading. A standalone shell has no members, so it uses the deepest registered repo holding its cwd.
- The readings of one link are a **probe group**: they are sent one at a time, and a reading the host rejects before the transfer starts (no such file) moves on to the next. A reading the host starts is the link's file, and its later failure is reported as any fetch's. The saved copy opens by the `:line` rule above.
- A link shows one toast for the whole group, `Fetching <file> from <host>`, updated in place as readings advance. When every reading fails it becomes one `Couldn't fetch <file>` error carrying the last error; a reading missing before the transfer starts shows nothing of its own. With no reading at all, or a pane with no session, nothing is sent and a warning `Couldn't fetch <file>` says `Not inside a repo or worktree of this session.`
- The ⋯ menu's **Fetch file…** row, right after Shell… (selector `sidebar-more-fetch-file`, no shortcut), shows only on a remote host with a focused pane and opens a modal (`fetch_dialog.rs` the model, `fetch_dialog_view.rs` the view).

  The scope is a repo root of the focused session, read once when the popup opens: a muted caption naming the repo for one root, a button per root for several (default the root holding the session's cwd), and "No repo for this session" with Fetch disabled for none. The field is relative to that root (placeholder `src/main.rs:42`, hint "Relative to <repo>. Add :line to open at a line.").
- The typed path is read like a link's: a trailing `:line[:col]` is split off (`links.rs` `split_path_position`), `\` becomes `/` and `.` segments are dropped. An absolute host path under the chosen root has the root stripped; one outside it is an inline error naming the root, and an empty path or a `..` segment is refused inline. Path problems show on Enter and clear on the next edit; Fetch stays disabled until scope and path are valid.
- The popup shows the fetch as a 6 px progress bar with "Waiting for <host>…" until the size is known, then "<n> of <size>". Errors (the daemon's, a refused download path, "Already fetching <file>") show inline under the field and leave the popup open; while the popup shows a fetch it pushes no `Fetching` toast and no failure toast.
- Cancel and Esc cancel the fetch (`CancelFetch`, the part deleted, no toast) and close; a backdrop click does nothing. Once the last chunk has reached the writer the fetch is finishing: Cancel is disabled and Esc ignored, and on the save the popup closes and then opens the file (so a run confirm can show). A lost connection closes the popup with every other dialog under the connection overlay (the part deleted, no toast); closing it any other way cancels its active fetch first.
- A fetched file opens through its own route (`Origin::Fetched` in `open_view.rs`), checked to lie under the download root, while every other local open on a remote host stays refused.

  It then opens as a link does: in a viewer tab when `decide` says viewable, else in VS Code at `path:line:col` when a line is given, else in the default app, with the run confirm for files that run code. Re-fetching a file whose viewer tab is open re-reads it. A relative `RUSTLING_TULIP_REMOTE_FILES_DIR` is made absolute at startup.
- **Page media**: when a fetched `.html` or `.htm` that opens when done is saved, the client also fetches the relative media files the page names (`html_media.rs` `page_media`: `src`, `srcset`, `poster`, `data` and `link` `href` attributes, and `url()` and `@import` in `<style>` and `style=`) into the download root at the same repo-relative place, so the page opens with its media beside it. This is the media batch, and the page opens only after every media fetch has ended.
- A reference resolves against the page's folder and must stay inside the repo root; `data:`, `http(s):`, `//` and site-absolute `/x` references are skipped. A stylesheet the page links is read one level for its own `url()` and `@import` (`stylesheet_media`).
- At most 4 media fetches are in flight. Caps: 200 references per page, 25 MiB per file, 256 MiB per batch. A file over its cap is cancelled with `CancelFetch` and counts as skipped; once the batch cap is hit, the rest and any a later stylesheet names are skipped.
- A media file another page's batch is already fetching counts as done; a file already on disk is fetched again. Clicking the page again while its media fetch runs toasts `Already fetching <page>`.
- One toast covers the batch: `Fetching <page> and its media (n of m)`, replaced in place by Info `Fetched <page> and its media`, or by one Warning `Some of <page>'s media didn't load` ("{n} of {total} files failed or were skipped."). A media file never gets its own toast.
- A lost connection mid-batch deletes the media part files, does not open the page and shows one Error `Couldn't fetch <page>'s media` ("The connection was lost."). A page saved after its connection closed opens without its media, with that message as a Warning.

## Window

The window's restore rect, maximized flag and monitor are saved in native-ui.json and restored on that monitor (the primary when it is gone); fullscreen is not restored. The title reads `(N) Tab — rustling-tulip` with N the active tab's sessions that need you (`Tab — rustling-tulip` at zero), or `(busy/total)` / `(idle/total)` in those count modes, set 350 ms after its last change. Closing the window runs the quit flow (keep running, stop keeping worktrees, stop removing them with per-session branch fate, or abandon), waiting for `shutdown_ack`.

Closing a tab, closing a pane but keeping its session, and moving a pane to an existing tab each leave an entry on the undo shelf (bottom centre, 8 s, at most 3, newest on top).

An entry holds a whole-tab snapshot of every tab it touched, and Undo sends `RestoreTabSnapshot` for a tab that still exists or `RestoreTab` at its old index for one that is gone. A tab this client has asked to remove (closing it, closing its last pane, moving its only pane out) counts as gone from the send until its removal arrives, so an Undo pressed inside that round trip still restores it, bringing back splits and ratios exactly.

Moving the only pane via "Move this pane to a new tab", or via the Move-panes dialog with every pane ticked, marks the source tab closing the same way. A refused `MovePane` or `ExtractToNewTab` clears the mark, so Undo sends `RestoreTabSnapshot` and spawns aimed at the tab go into it. A pane whose session has since gone comes back empty.

The shelf clears when the connection drops or a new `Welcome` arrives.

## Windows and pop-outs

A pop-out is an extra window of the one client process: one daemon connection and one `RootView`, and each pop-out's root (`PopoutView`, `popout_view.rs`) holds a weak handle to it and draws from it, so every window whose last draw read the `RootView` repaints with it. A pop-out holds a pane, a whole tab (a grid or a diff) or a session that has no pane. The wire does not change, since the daemon's layout still holds a popped pane in its tab; the open pop-outs live only in `native-ui.json`.

`popouts.rs` keys a pop-out by what it holds: `Pane { tab_id, pane_id }`, `Tab { tab_id }` or `Session { session_id }`, kept in open order. Opening one again raises the first. A pop-out that would share content with an open one raises that one instead and its Pop out row is dimmed "Already in a pop-out": a session already shown by a pane or session pop-out, or a pane and the tab that holds it.

A viewer tab is refused ("Viewer tabs can't be popped out"). A pane keeps its id when it moves in the main window, so its pop-out follows it to its new tab.

A pop-out opens 1100×720 (minimum 700×400), centred on the main window's display; each further one opens 32 px right and down of the one before it. Its title is the session's label or the tab's name, and a session pop-out whose session the daemon has removed reads "Session not found". Only the main window's close runs the quit flow; closing a pop-out docks it back.

Dock back is the close of every kind: a pane returns to its grid, a tab's pill loses its mark, a session pop-out simply closes. It sends nothing, so it never stops a session or removes a tab. A pane pop-out closes by itself when its pane leaves every tab, and a tab pop-out when its tab does; a session pop-out stays open after its session stops. Pop-outs have no app shortcuts and no Pop out button of their own.

The main window keeps the popped content's place as a card, which is not a terminal and holds no keyboard. A popped pane's grid slot draws a card with the session's label, "Popped out to its own window", Focus window and Dock back; a popped tab draws the same card over its body while it is the active tab, and its pill shows a muted ↗ ("Popped out"). Focus window raises the pop-out and focuses its terminal or diff.

A pane pop-out's toolbar has the status glyph, the label, the tab it came from, the member, runtime and trusted chips, a two-step Stop and Dock back.

A tab pop-out has its own toolbar (the tab's name, Dock back) and no tab strip; it draws the tab's whole grid, with splits, divider drags, pane drops and menus working inside it, or a diff tab's own view, which moves over without a refetch. In it Pop out on a pane is dimmed, and empty panes have New session… and Shell here dimmed "Spawn from the main window". A session pop-out has Stop and Close window.

A pop-out's terminal takes part in `tabs::size_drivers`, so only one view of a session answers terminal queries, and a popped tab's panes are sized and focused from the pop-out whichever tab the main window shows. The grid's bounds (`window_layouts.rs`) and the drag state are keyed by window. A drag stays inside the window it started in: on Windows the press captures the mouse to that window, so panes move between windows by Dock back and Move to ▸, never by a cross-window drop.

A pop-out's terminal or diff takes its window's keyboard when the window opens and on Focus window. Docking back a tab that is the main window's active tab returns the keyboard to its focused pane or diff. Showing a session that a pop-out holds (a leaf click, a jump) raises that pop-out and clears attention instead of placing the session again, and a session shown in the active pop-out's focused terminal counts as seen.

Menus, dialogs and toasts open in the window that asked (`overlay_owner.rs` records each overlay's window, and a closing window drops its overlays). A toast shows in the window the user last interacted with, else the main window.

`native-ui.json` keeps the open pop-outs in open order, each with its key and its window's place (`window_state.rs`), saved as the window moves or resizes with the layout's 500 ms debounce. Once per launch, when the boot target's tabs and sessions have both arrived, the client reopens them in saved order behind the main window at their saved places, on the primary display when the monitor is gone and at the cascade place when the entry never moved.

A restore leaves the keyboard in the main window and raises it only if it was active, and an entry whose pane, tab or session is gone is dropped (a pane that moved reopens under its new tab). A reconnect reopens nothing, and a switch of host skips the restore until the first pop-out opens or closes there.

Quitting closes the pop-outs with the app after saving the open set, and the exit dialog counts sessions only. Closing every window, the main one included, keeps the pop-outs for the next launch. A missing `popouts` key or a value that is not a list loads as empty, and an entry whose key does not decode is dropped alone.

## Keyboard reachability

Settings → Appearance and the appearance editor share one set of keyboard stops. Tab and Shift-Tab leave the hex and filter fields for the next or previous stop, as the Worktrees path field does, rather than staying inside the field.

Stops run in reading order, column 1 then column 2: per colour section (Accent, then Background) the preset swatch line, the recent swatch line (only when recents exist), the hex field, Apply (while enabled) and the reset link (while enabled); then the font filter and the family list; size −, size + and the size reset link; the bold checkbox and the bold reset link; then Close, in the session, repo and workspace editor only.

Disabled stops are skipped. The family row has no reset link; the list's Reset row covers it.

Each swatch line is one stop, landing on the chosen swatch, else the first; Left and Right move along it and stop at the ends without wrapping, Enter or Space picks. The family list is one stop, landing on the chosen family, else the first row: Up and Down move a highlight that skips headers, stops at the ends and scrolls into view, Enter or Space picks. After a filter edit the highlight stays on the same family while it is listed, else moves to the first row.

A keyboard-focused repo, workspace or tab row opens its container menu with Shift+F10 or the Menu key, anchored at the row's bottom-left with the first enabled row highlighted. Up and Down move through enabled rows and wrap, Home and End jump to the ends, Enter or Space runs the highlighted row (the same action a click runs, `container_menu_rows` in `session_menu.rs`), Tab does nothing, and Escape closes the menu and returns the keyboard to the row. A right-clicked menu opens with no highlight; the first Down highlights. Folding the sidebar closes an open container menu, and the menu reads keys only while it holds the keyboard.

Picking a swatch, by mouse or keyboard, adds its colour to the recent colours (deduped, at the front) once the level takes the change; a recent swatch's tooltip reads "Recent color #rrggbb". A press that disables its own stop (Apply, a reset link) hands the keyboard to the next enabled stop after it. A click on any Appearance control moves the keyboard stop there, and closing Settings clears its stop.

The editor walks the same stops, Close included, and draws the focus ring on its focused stop. It opens with no stop focused: Tab lands on the first stop and Shift-Tab on Close. The ring wraps, from Close to the first stop and from the first stop back to Close. Enter or Space on Close closes the editor, Tab leaves the hex and filter fields with the typed text kept, and Esc closes the editor whatever stop has focus.

The stop order, ring step (`step`), where focus goes when a press disables its stop (`after_lost`, shared by Settings and the editor), cursor and highlight logic is plain Rust in `appearance_stops.rs`, unit-tested; Settings keeps its one focus slot, `settings_control`.

When a daemon echo or re-read removes the focused Settings control from the Tab ring (Worktrees Save or Reset, Remote access Apply or Autostart, an agent field a rule disables), the keyboard moves to the next stop in that page's full order (`settings_stop_order`) instead of falling back to the tab list; after a Worktrees Save it lands on Reset. If the user has moved focus elsewhere, nothing moves. The rule runs at render time (`RootView::render` calls `keep_settings_keyboard`).

A text field without the keyboard draws no selection. A field built with its text selected shows the selection once it takes focus, and typing still replaces the text (`text_input.rs`, `painted_selection`).

Menus and modals keep to the window and hand the keyboard back. A context menu opens at its anchor and snaps inside the viewport, and its frame never grows past 60% of the window's height: a longer menu scrolls its rows instead (`session_menu.rs` `menu_frame`, `MENU_MAX_SHARE`). Confirm dialogs open on their safe option — Cancel, Keep running, Ignore, or a pane-only close — and every close path, Esc included, returns the keyboard to the pane.

Esc has deliberate exceptions. It never dismisses the layout chooser (the import modal in "Tab layouts" does: Esc and its ✕ both keep the layout), and it is ignored while a recovery or an exit is in flight. In a session rename it leaves the menu open and returns to its rows rather than closing them.

## Tests

Three tiers, all Rust tests:

- **UI specs** (`apps/native/tests/ui/`, one binary with a module per spec file): in-process, headless GPUI through the `test-support` feature (`VisualTestContext`), never touching the user's mouse or keyboard. `RootView::with_transport` takes the `NetCommand` / `NetEvent` channel pair, so specs script a fake daemon (`tests/support/mod.rs`), inject `DaemonMessage`s and assert on the `ClientMessage`s sent, grid text, selection and clipboard.

  `ui_dir` and the clock are injected, so specs never write the real native-ui.json. The test platform shapes text as fixed 0.6 em glyphs, so specs assert on cells, never on font pixels. Specs that rely on focus events call `window.activate_window()` first.
- **Live e2e** (`tests/e2e_live.rs`, `tests/e2e_recover.rs`, `.\rt.ps1 native-e2e`): the same harness against a real daemon whose config, binaries and worktrees dirs are all under `.tmp/`, set on the child with `Command::envs`, never process-wide. Specs create sessions through a clone of the client's `NetCommand` sender. A spec using a fake agent CLI (fake-claude for Claude) with no `node` on PATH fails, never skips.
- **OS smoke** (`tests/smoke_window.rs`, `.\rt.ps1 native-smoke`): launches the real exe with `RUSTLING_TULIP_OFFSCREEN_WINDOW`, which opens it cloaked and never activated, so it never takes focus or covers the user's work. Before launch the spec writes the client's `client-id-native` and lays out its sessions for that id over its own socket, so the first-connect chooser never dims the probes.

  Input is posted to the HWND, each key only after the shell has echoed the previous one: an attach probe posts Space until it echoes and the screen holds still, then each letter waits until the typed line equals the text so far (a Space waits for scrollback growth), and the whole line is checked before Enter.

  Posted key-downs are not ordered like a real keyboard (Space and Enter are sent at key-down while letters arrive later as `WM_CHAR`), so a stalled UI thread let them overtake queued letters. Every wait in the spec is a 120 s hang guard, never a timing assumption.

  `smoke_keys_posted_while_the_client_stalls_reach_the_shell_in_order` suspends the client's UI thread (`SuspendThread`) right after the `o` of `echo` is posted, so the Space after it must wait for the thaw, which reproduces a load-only fault without a CPU burn.

  Pixels are read with `PrintWindow(PW_RENDERFULLCONTENT)` from the threaded show and probed for known UI colours and for a change in the pane after typing; the spec checks the window is still cloaked and not foreground after each capture.
- **Screenshots** (`examples/shot.rs`, `.\rt.ps1 native-shot [<view>] [-Out <path>]`): not a test tier but the way to look at a change.

  It opens the real window cloaked over `RootView::with_transport` and a fake daemon scripted in the example (four repos, a workspace, seven sessions in every status, a split tab, a diff tab), drives it to the view with the app's own keys (a posted click for the rail), waits for two equal captures (5 s at most), and writes `.tmp\shots\<view>.png` with the smoke tier's `PrintWindow` capture.

  No daemon starts and no config dir is read. The Working spinner animates on wall-clock time, so two shots differ inside that glyph and nowhere else.

The e2e and smoke tiers are `#[ignore]`d in the normal nextest run and run only through their `rt.ps1` verbs, under `nextest --run-ignored only`. A probe or dev daemon never runs with default dirs, where its client attaches to the user's real sessions and resizes their PTYs: it sets all of `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR` and `RUSTLING_TULIP_WORKTREES_DIR` to `.tmp/` dirs, as `native-e2e` does.
