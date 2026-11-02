# On-disk layout

Where the daemon and the native client keep their files, what each file holds, and how they are written. The rules for changing a saved file's shape are in [CLAUDE.md](../CLAUDE.md), "Saved user data survives every update".

## Config dir

Both sides resolve the config dir via the `directories` crate as `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`. On Windows that expands to `%APPDATA%\leftos\rustling-tulip\config\` (note the `leftos\` + `\config\` segments — `directories` inserts them, so a plain `%APPDATA%\rustling-tulip\` path is wrong). Layout under that root:

- `state.json` — persisted repos + workspaces + tabs, plus daemon-side host settings that must apply with no window open (see `crates/daemon/src/state.rs`).

  They are `worktrees_root_override`, `keep_awake_mode`: `"off"`, `"when_busy"` (the default) or `"always"`, `agent_commands`: `{"<agent serde name>": {"program", "args"}}` for the Settings launch command, only agents that set one, a blank pair removing the entry, `agent_settings`: `{"<agent id>": {<option>: <value>}}`, each agent's validated Settings page values.
- `state.json.corrupt-<UTC stamp>` — a `state.json` that failed to parse, moved aside (or its bytes written beside it) before the daemon starts on defaults; the 3 newest are kept, and the secret sweep counts the `${secret:<id>}` references a text scan finds in each, so no sealed value they name is deleted (`crates/daemon/src/state.rs` `keep_corrupt_state`, `crates/daemon/src/main.rs`).
- `daemon.json` — handshake (port + auth_token + pid + supported_versions + build_version, the daemon's package version); written on daemon start, removed on graceful shutdown.
- `daemon-id` — this daemon's stable id, a bare UUID with no newline, sent in `Welcome.daemon_id`. Created on the first start; a file that is empty or not a UUID is replaced with a new id (warn "the daemon id file is empty or not a UUID; replacing it with a new id"), and one that cannot be read or written fails startup with "loading the daemon id" (`crates/daemon/src/daemon_id.rs`, `paths.rs` `daemon_id_file`).
- `sessions/<id>/meta.json` + `scrollback.bin` — orphan-recovery sidecar and PTY scrollback ring. `hook-settings.json` sits beside them for an interactive Claude spawn (passed as `--settings`) and for a recovered shell with a hook agent and a conversation id; a graceful stop deletes it with the sidecar.

  `summary.json` sits beside `meta.json` too: the session's summary ring as a protocol `SessionSummary` whose `lines` is the whole ring (at most 12 lines, none older than 2 hours, newest first). It is written atomically right after each ring update and only while `meta.json` exists (it never creates the folder), restored by the orphan, reattached and abandoned inserts (a missing, corrupt or emptied ring restores no summary), and deleted with the sidecar (`crates/daemon/src/summarizer/turns.rs`, `crates/daemon/src/session.rs`).

  The sidecar's `resumed_from` (absent when empty) holds the session ids a resume replaced, oldest first, the newest 8, restored by the orphan, reattached and abandoned inserts; it is set at spawn and never changes.

  The sidecar's `program_name` follows the record (the shell's label once an agent typed into its shell has ended, the agent again on its next `SessionStart`), and `agent_in_shell` and `shell_label` mark a session whose PTY child is the user's shell, so a reattach runs the shell's status watcher (absent means the agent is the child).

  A recovered shell's `hook_agent` (the plugin that maps its hook events; absent means none, and the hook route then maps nothing for the session), `hook_options` (the recovered selection's options, an empty object by default) and `hook_trusted` (whether the recovered run was trusted, false by default) are stored there too, written once at spawn, and restored by the orphan, reattached and abandoned inserts; the shell's hook events are mapped with them, not with its own spawn config.
- `remote.json` — opt-in tailnet access switch `{enabled, port}`, absent until remote access is turned on; the daemon writes it (temp file and rename) whenever `ConfigureRemote` changes it, and an identical request writes nothing; a missing, unreadable or zero-port file means off, and the port defaults to 8787 (`crates/daemon/src/remote_config.rs`).
- `history/<id>.json` — session history: one entry per ended session, holding how it ended, its spawn config and folders, its `busy_excluded` flag when true, its `agent` (absent for a shell) with the `options` and `trusted` flag an import recorded, and its conversation id `conversation_id`. A tracer-log import infers a DeepSeek `provider` option from its `--model`.

  For a shell-hosted agent it reads the agent note in the tracer log instead, giving the entry that agent with its options, trust, extra folders and conversation id, and falls back to the plain-shell entry when the agent is unregistered or the note unreadable.

  An entry is written on every end path, written at startup for each dead non-headless sidecar that has none (a session a reboot, a crash or a killed process tree ended: `TracerLost`, or its true end when the tracer log shows one), backfilled at startup from `logs/tracer-*.log`, and pruned after 7 days (`crates/daemon/src/history.rs`).

  The recover dialog lists these and `RecoverSessions` respawns them: an agent entry with its plugin's resume (Claude: `claude --resume`), as `RecoverAs::Agent`, `RegisterRepoThenAgent`, `ShellResume` (a shell typing the resume) or `Shell` (a bare shell).
- `contested/<agent id>.json` — the conversation ids that discovery left unclaimed because two or more sessions waiting in one folder were each eligible for them: a JSON array of `{id, contested_at}`, one file per agent, written atomically when an id becomes contested, entries older than 7 days dropped, the directory created on first write; a stored id is never claimed (`crates/daemon/src/conversations/contested.rs`, `Dirs::contested_dir`).
- `activity/session-<id>.jsonl` — the status-change log the Dashboard's two-hour activity track reads: one `{at, status}` line per status change and per insert, lines older than 2 hours pruned at start and every 60 s (the newest older line kept, so the status at the window's start is known), a file deleted once its session has no sidecar and its newest line is 2 hours old. `activity/daemon.json` holds this daemon's `started_at`, an `alive_at` stamped every 60 s and the restart gaps inside the window (`crates/daemon/src/activity_log.rs`, `Dirs::activity_dir`).
- `env-secrets.json` — index of the secret env-row values sealed into Windows Credential Manager (`{id, key, created_at}`, never a value); the values are Credential Manager entries under the service `rustling-tulip` (`rustling-tulip:<8 hex of the config dir's SHA-256>` while `RUSTLING_TULIP_CONFIG_DIR` is set), user `env/<KEY>/<id>`. Stored configs carry `${secret:<id>}` references instead (`crates/daemon/src/env_secrets.rs`, `docs/env-secrets.md`).
- `state.json`'s `summarizer` block — `{enabled, provider, key_source, daily_cap_usd}`, secret-free and absent until first saved; the defaults are built at read time (off, the first registered CLI summarizer, a $1.00 cap, the `stored` key source) (`crates/daemon/src/state.rs`).
- `summarizer-spend.json` — the summarizer's spend ledger, `{"date": "YYYY-MM-DD", "spent_usd": n}` for the current local day, written atomically (`crates/daemon/src/summarizer/ledger.rs`, `Dirs`).
- `day-ledger.json` — the Dashboard header's day figures, `{"days": {"YYYY-MM-DD": {"sessions": {"<id>": {base_cost_usd, base_tokens, cost_usd, tokens}}, "commits": [sha…]}}}`, keyed by local calendar date. A session's figures only rise, and its day figure is current minus base. Commits since local midnight are read per session worktree, deduped by sha and read at most every 60 s per folder. Days older than 30 are dropped on each write, and it is written atomically at most every 10 s while a Dashboard is watched. A file that does not parse is renamed `day-ledger.json.corrupt-<UTC stamp>` and the ledger starts empty (`crates/daemon/src/dashboard/ledger.rs`, `paths.rs` `day_ledger_file`).
- The Dashboard summarizer's API keys are Credential Manager entries under the same service naming, one per provider, user `summarizer/<provider id>`; they have no index entry (`crates/daemon/src/summarizer/keys.rs`, `docs/plans/dashboard.md`).
- Device tokens (native client, one per remote host) are Credential Manager entries under the same service naming, user `device/<lower-cased host>` (no port); a host with no entry is dialled with an empty token (`crates/daemon-client/src/device_tokens.rs`, `CredentialManagerTokens`).
- `devices.json` — the tailnet devices approved to connect: their SHA-256 token hashes, never a token, with each device's `last_seen` and its `revoked` flag (a blocked device); `ForgetDevice` removes a record (`crates/daemon/src/devices.rs`, `DeviceStore`).
- `daemon.lock` — the single-instance lock; a second daemon on the same config dir exits without touching anything.
- `logs/daemon.log` — daemon tracing output. Rotated on each daemon start: the previous run survives as `daemon.log.old`; while it runs, the file rolls to `daemon.log.1` each time it would pass 5 MiB (`daemon_client::capped_log`), and the tracer-log import reads all three (see `crates/daemon/src/main.rs::init_tracing`).
- `logs/autostart.log` — the login launch's short log: `rustling-tulipd --detach` (the command the HKCU `Run` value `rustling-tulip-daemon` starts at login, written by `daemon_client::autostart`; the client only adds `--detach` to an entry that already names its own daemon and never repoints one) copies itself into the binary cache, starts that copy and exits (`crates/daemon/src/detach.rs`). Truncated on each such launch.
- `logs/native.log` — native client (`apps/native`) tracing output, also mirrored to stderr (on Windows only a debug build has a console to show it: a build without debug assertions is a GUI-subsystem exe, so an installed launch opens no console window). Rotated on each launch to `native.log.old`, and rolled to `native.log.1` each time it would pass 5 MiB while the client runs.
- Outside the config dir, the installed native client (the installer's `uninstall.exe` beside the exe) writes its toast registration at each start: the Start menu shortcut `%APPDATA%\Microsoft\Windows\Start Menu\Programs\rustling-tulip.lnk` (app id `leftos.RustlingTulip`, activator CLSID) and the registry key `HKCU\Software\Classes\CLSID\{354BE9DC-6D82-4269-8883-D13CE3E38648}\LocalServer32` (`"<exe>" -ToastActivated`); the uninstaller deletes the key (`apps/native/src/toast_activation.rs`).
- `driver-shots/<n>.png` — the driver endpoint's `screenshot` captures of a driven run, `<n>` counting from 1 in each run (`apps/native/src/driver/endpoint.rs`, `docs/native-client.md` "Driver endpoint").
- `themes/<slug>.json` — the native client's user themes, one per file (`{name, base?, terminal_source?, ui, terminal}`, every colour present); `ThemeStore::save` writes `.<slug>.json.tmp` and renames it into place and never overwrites an existing slug; the selected one is `native-ui.json`'s `theme`, and the boot saves "My colours" there from a released file's app colours.

  A `*.json` there whose name is not a valid id, or is a built-in theme's id, is listed with an error and left unread, and a failed save removes its `.<slug>.json.tmp` (`apps/native/src/theme_store.rs`, `docs/native-client.md` "Theme and fonts").
- Outside the config dir, a driven client (`RUSTLING_TULIP_DRIVER=1`) writes its discovery file `%TEMP%\rustling-tulip-driver\<pid>.json` (`{pid, pipe, started_at, version}`) once its cloaked window has drawn, and removes it on quit; stale files there are swept at the next driven start (`apps/native/src/driver/endpoint.rs`).
- `client-id-native` — per-install client identity (a bare UUID) that the native client sends in `Hello`; tab layouts are keyed by it.
- `native-ui.json` — native client UI state that stays on this machine (`apps/native/src/sidebar.rs`). It holds the sidebar width, the sidebar's collapsed flag and the collapsed containers (a host group's fold too, as `host:local` or `host:<host key>`), the Repos view's Active filter (`repo_filter`, `"all"` default or `"active"`; a missing key or unknown value loads as `"all"`), the active tab, the quick-shell folder, the folder Add repo opens at (`last_repo_dir`) and the folder the workspace creator's `.code-workspace` picker opens at (`last_vscode_dir`).

  It also holds the app-level terminal font and the interface font (`ui_font_family`, absent for the bundled Inter).

  It holds the interface text size (`ui_text_size`, a whole number from −2 to +4, absent means 0, out of range clamps on load, a non-whole number loads as 0 with a warning), the interface weight (`ui_font_weight`: 400, 500 or 600, absent means 400, anything else loads as 400 with a warning) and the app-level terminal frame width (`terminal_frame_width`: whole px 0 to 6, absent or null means the built-in 1 px, anything else loads as absent with a warning).

  It holds the theme (`theme`: a built-in's id or a user theme's file slug, absent or null for the default built-in, Nord; read at boot, written by the Themes modal's Use) and `app_appearance` (the app accent and terminal background that releases up to v0.5.2-alpha saved; read only so the boot can move them into a user theme with `theme_store::migrate_app_colors`, absent once moved).

  It also holds the shared list of recent custom colours, per-tab font sizes, the activity rail's view (`activity`), and the source-control panel's pinned repo, collapsed sections and split heights. For a remote host's repo the pinned repo is `{host key}::{repo id}` and a section's `collapsed` key and `history_list_height` key carry the same prefix.

  It holds the diff tabs' include-whitespace and highlight toggles, whether a Markdown viewer tab opens rendered (`viewer_markdown_rendered`, default true; a missing key loads as true), and the per-host remote session counts (`host_session_counts`, `{"<lower-cased MagicDNS name>": <count>}`, or `"ssh:<host string>"` for an SSH host, only for hosts in `saved_hosts`; kept from each saved remote host's own session list, for a switched-off host's "N sessions last time").

  It holds the open pop-out windows in open order (`popouts`, each `{"key": {"kind": "pane"|"tab"|"session", …ids}, "window": <the window shape, or null until first moved>}`; a session key also carries `"host": "{host key}"` for a remote host's session and omits it for This PC; restored once per launch once the tabs and sessions arrive; a missing key or a non-list value loads as empty, and an entry whose key does not decode is dropped).

  It holds the Settings modal's Spawn default (`spawn.trusted`, the trusted-launch default; Claude's approval mode and provider are the daemon's `agent_settings`, not stored here), `spawn.last_trusted` (the Trusted choice of the last Spawn from a dialog the user filled in for an agent, not a prefilled one or a Shell spawn; absent until then, and then the New session dialog opens on `spawn.trusted`), General, Notifications and App title choices (`general`, `notifications`, `title`).

  General includes `general.check_updates` (default on; off stops the updater's automatic checks and downloads), the sidebar's leaf density (`general.leaf_density`), the sessions it draws under repos (`general.sessions_under_repos`: `"every"` default, `"busy"`, `"none"`), how it sorts repos (`general.repo_sort`: `"needs_you"` default, `"active"`, `"manual"`; a missing key or unknown value loads as `"needs_you"`). `general.open_text_externally`, which releases up to v0.6.1-alpha wrote, is read and ignored: Ctrl+click and Ctrl+Shift+click choose the app instead.

  Notifications holds the `Needs you` and `Finished` toggles (`notifications.awaiting_input` and `notifications.finished`) and the skip choice.

  It holds the Needs you sound (`notifications.needs_you_sound`: `"off"`, `"default"` or `{"custom": "<path to a .wav or .ogg>"}`; a missing key or an unknown value loads as `"default"`, the bundled `apps/native/assets/sounds/needs-you.ogg`).

  It holds the Finished sound (`notifications.finished_sound`, the same shape: `"off"`, `"default"` or `{"custom": "<path to a .wav or .ogg>"}`; a missing key or an unknown value loads as `"default"`, the bundled `apps/native/assets/sounds/finished.ogg`).

  It also holds the toast style (`notifications.toast_style`: `"os"`, the default, or `"app"`; a missing key or an unknown value loads as `"os"` with a warning and the rest of the file still loads; both values raise the OS toast until the Settings row and the app's own toast windows land).

  Its `saved_hosts` list (the hosts the host manager offers) is what the client connects at launch: This PC, then each enabled saved host in saved order. The host manager writes it (adding or picking a host saves it on, a row's switch sets its `enabled`, and Forget rewrites the list).

  Each entry has a `kind`, `"tailnet"` when missing: a tailnet entry is `{name, port}` (written without `kind`; one that fails the DNS-name check is dropped on load), an SSH entry is `{"kind": "ssh", "name": "<host string>", "remote_command": "<optional>"}` with no port. An entry may carry `"enabled": false` for a host switched off; only a disabled host writes the key, and a missing key means enabled. An unknown kind, or an SSH host string that fails its check, is dropped on load. The SSH add form (`Add and connect`) writes an SSH entry (with no port), re-adding a name replaces the whole record, and Forget removes it.

  Its `session_views` map holds each session's remembered pane view per host: `{"<host key>": {"<session id>": "chat" | "summary"}}`, the host key `"local"` for this PC, else the key `host_session_counts` uses; only chat and summary entries are written (a terminal is the default), a host with none is left out, and a save prunes only the connected host's ended sessions, once its session list has arrived (a missing or malformed value loads as empty, with a warning for a wrong type).

  Its `unc_hosts` list (network hosts terminal links may open) is edited by hand; the client re-reads it on each click and never writes it. The native client's bundled OFL fonts live in `apps/native/assets/fonts/` (sources and versions in its README), and the large-file hook exempts them. The attribution for the built-in themes' source colour schemes is `apps/native/assets/themes/NOTICE.md`.

When debugging spawn/connect/shutdown issues, both `daemon.log` and the client's log (`native.log`) together tell the full story — neither alone is enough.

## Worktrees and machine-local roots

Worktrees live under a **separate** root resolved via `ProjectDirs::data_local_dir().join("worktrees")` (overridable with `RUSTLING_TULIP_WORKTREES_DIR`). On Windows that's `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` — machine-local, doesn't roam, and known-writable regardless of where the source repo lives.

Per-session worktree paths are `<worktrees-root>/wt.<branch-slug>/<repo-slug>` for a single repo and `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/<offset>` for a workspace member, where the **anchor** is the common path-component prefix of the member repos' *parents* on `members[0]`'s drive and `<offset>` is each member's path below it.

This preserves inter-member relative paths inside a workspace session — if `repo1` references `repo2` as `../repo2` in source space, the same reference resolves inside the worktree set.

Members on another drive are anchored per drive under `<workspace-slug>/<drive anchor>/<offset>`; that folder's first segment takes a `-2`, `-3`, … suffix when the plain name would equal a primary-drive member's path or contain one (`…\ws\Y-2\b`). Slugs come from `git::name_slug`; repos and workspaces share one slug namespace, and saving a name whose slug is taken is refused.

A group folder has a `<slug>.rt-group` marker beside it (`{"kind","name"}`), which Manage worktrees reads to list each group separately; a group's members are its linked worktrees, a member nested inside another included (a submodule or plain clone inside one is not listed), and a workspace marker makes a one-member group launch as that workspace.

A spawn reuses the worktree git reports as holding the branch, skipping one git marks prunable (its folder is gone): that stale registration alone is removed with `git worktree remove --force` before the add or a Recreate, and a spawn never runs `git worktree prune`, which would also drop a worktree on a detached drive (any layout, so older `wt.<branch>/<sanitized-anchor>/…` worktrees are still reused) and refuses a folder at its path that isn't one of the repo's worktrees. Path construction lives in `git::workspace_worktree_paths` / `git::single_worktree_path`.

The tailnet listener's certificate lives in a third machine-local root beside the worktrees one, `ProjectDirs::data_local_dir().join("tailnet")` (overridable with `RUSTLING_TULIP_TAILNET_DIR`), `%LOCALAPPDATA%\leftos\rustling-tulip\data\tailnet\` on Windows: `<name>.crt` and `<name>.key`, written by `tailscale cert` for the host's MagicDNS name (no trailing dot).

It is not under the config dir, so the private key never roams (`crates/daemon/src/paths.rs` `resolve_tailnet_dir`, `crates/daemon/src/tailnet_listener.rs`).

Files the native client fetches from a remote host live in a fourth machine-local root beside those, `daemon_client::remote_files_dir()` (overridable with `RUSTLING_TULIP_REMOTE_FILES_DIR`), `%LOCALAPPDATA%\leftos\rustling-tulip\data\remote-files\` on Windows: `<host MagicDNS name, lower-cased>/<repo-name>/<rel-path>`, with Windows-unsafe names sanitised; an SSH host's folder is `ssh-<host string, lower-cased and sanitised>` (`ssh-me@devbox_2222`).

Auto-update downloads live in a machine-local root beside the others, `%LOCALAPPDATA%\leftos\rustling-tulip\data\updates\` on Windows (overridable with `RUSTLING_TULIP_UPDATES_DIR`): one `<version>-<random>\` folder per download, holding the verified installer, and `last-result.json` (`{from, message, ok, to}`), which `rustling-tulipd apply-update` writes and the client reads once after the restart.

Older folders and the `.old` files an update left in the install folder are deleted at the next launch (`apps/native/src/update.rs`, `docs/auto-update.md`).

The Dashboard summarizer runs the agent's CLI in a fifth machine-local root beside those, `ProjectDirs::data_local_dir().join("summarizer")` (no override variable), `%LOCALAPPDATA%\leftos\rustling-tulip\data\summarizer\` on Windows: nothing lives there but the CLI's working folder, which is kept empty and never emptied by the daemon. It is created on first use (`crates/daemon/src/paths.rs` `resolve_summarizer_dir`, `crates/daemon/src/summarizer/cli.rs`).

A fetch writes a hidden `.fetch-<launch stamp>-<n>.part` beside the target and renames it over the target when the file is done; a failed or cancelled fetch deletes its part. Nothing under it is ever pruned (`apps/native/src/file_fetch.rs`).

A fetched `.html` or `.htm` page that opens when done brings the relative media it names with it, saved at the same `<host>/<repo-name>/<rel-path>` place (never outside the repo root) and each fetched again if already on disk (`apps/native/src/html_media.rs`, `docs/native-client.md` "Fetched files").

## Core files and atomic writes

The daemon's core files under the config dir (see `crates/daemon/src/paths.rs`):
- `state.json` — repo + workspace registry plus host settings (never sessions)
- `daemon.json` — handshake (port, token, pid, supported protocol versions, build version)
- `daemon-id` — the daemon's stable UUID, written atomically on first start
- `sessions/<id>/meta.json` — orphan-recovery sidecar; written at spawn before the session's first snapshot is sent (so a session any client has seen survives a daemon crash), deleted on graceful stop
- `sessions/<id>/hook-settings.json` — the hooks document an interactive Claude spawn passes as `--settings` (a recovered shell's `shell_resume` command names its path); written at spawn, deleted on a graceful stop
- `sessions/<id>/summary.json` — the session's summary ring (a protocol `SessionSummary`); written atomically after each ring update while `meta.json` exists, deleted with the sidecar
- `sessions/<id>/scrollback.bin` — 2 MB ring (trims to about 1.5 MB on overflow, cut at the next line start within 4 KiB, off the async workers), held open by the session's scrollback gate while it runs and replayed via `LoadScrollback` on attach
- `sessions/<id>/scrollback.truncated` — flag file iff the ring overflowed

Every temp-file-and-rename write in the daemon (sidecars, `state.json`, `daemon.json`, the indexes and ledgers above) goes through `atomic_file::write_atomic`: an owner-only temp file unique per call, flushed with `sync_all` before `std::fs::rename` replaces the target (retried briefly while another process holds it), so a power cut leaves the old bytes or the new ones, never a torn file.

Sessions are deliberately **not** in `state.json` — they're rebuilt from sidecars on startup so the daemon can survive restarts without a single fragile state blob.
