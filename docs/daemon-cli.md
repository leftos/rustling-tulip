# Daemon CLI and supervision

The `rustling-tulipd` subcommands, and how a client starts, retires or refuses a daemon.

## Subcommands

`rustling-tulipd pair` prints a one-time device pairing code from the running daemon (it needs the daemon already running, reads `daemon.json` and takes no instance lock; `crates/daemon/src/cli_pair.rs`); the owner types the code on a device whose pairing request is waiting.

`rustling-tulipd shutdown` stops the running daemon the way the client retires one: it sends `/shutdown` only to a listener that has proven it holds the token, leaves every tracer alive, creates no folders and starts no log (`crates/daemon/src/cli_shutdown.rs`, `daemon_client::shutdown_in`); the installer runs it on install and uninstall. The `/proof` key context and the `/proof` and `/shutdown` routes are therefore a contract with every released daemon.

`rustling-tulipd apply-update --from <staging> --to <dir> [--old-version <v>] [--update] [--relaunch]` is what the installer runs from the staged daemon to swap the installed files (`crates/daemon/src/cli_apply_update.rs`, `docs/auto-update.md`): it stops the daemon, renames each file to `.old` and the staged one into place, rolls every file back on a failure, writes `last-result.json`, and with `--relaunch` starts the client.

Its exit codes: 0 applied, 1 bad arguments, 2 the daemon would not stop, 3 an exe still runs from the install folder, 4 another update holds the lock, 5 a file failed and was rolled back. `rustling-tulipd --version` prints the daemon's version.

`rustling-tulipd bridge` is what an SSH client runs on the host: it relays one compact JSON protocol message per line between its stdin and stdout and the loopback daemon's WebSocket, starting the daemon first when none runs (`daemon_client::ensure_running`) and filling the `Hello` token from the host's handshake so the token never leaves the host (`crates/daemon/src/cli_bridge.rs`).

It exits 0 when the daemon closes the WebSocket, and after stdin EOF once the daemon has closed or 5 s have passed (it relays replies still in flight and sends its Close only then, since the daemon drops queued replies once it reads one); a daemon it starts on Windows inherits none of its standard handles. On failure it prints one stderr line `rustling-tulipd bridge: <error>`, leaves stdout empty and exits 1. It starts no tracing, takes no instance lock, rotates no log, creates no folders and attaches no console.

`rustling-tulipd screen <session id> [--fixture <path>]` shows how the running daemon reads one session's screen (it needs the daemon already running and proves the token like `pair`; `crates/daemon/src/cli_screen.rs`, the daemon's side in `crates/daemon/src/server/screen.rs`). It prints one line per rule (`<priority>  <name>  <region>  matched|vetoed by guard N|no match  <preview>`), `winner: <name> (<verdict>)` or `winner: none (no_match)`, any `rejected:` lines, `title: <title>` when there is one, then `grid <cols>x<rows>:` and the rows.

With `--fixture` it then writes the trimmed rows (LF) to exactly that path, refusing an existing file, and prints `wrote <path>`. It refuses at once against a daemon whose `Welcome.features` lack `screen_dump`, and on any failure prints one stderr line `rustling-tulipd screen: <error>` and exits 1.

### Session-driving verbs

`rustling-tulipd sessions` and `rustling-tulipd read` work against the running daemon over the protocol every client speaks (`crates/daemon/src/cli_drive/`, on the shared connection in `crates/daemon/src/cli_client.rs`). They create no folders, take no instance lock, start no log and never start a daemon. They act on whichever daemon `daemon.json` in the active config dir names, so `RUSTLING_TULIP_CONFIG_DIR` picks an isolated one and nothing else guards the choice. They send no `Resize` and no client id, so the user's panes and the user's clients' layouts stay as they are.

`rustling-tulipd sessions [--json]` prints one tab-separated line per session: id, status (`spawning`, `idle`, `working`, `awaiting_input`, `stopped`, `error`), `bg` or `-`, agent id or `shell`, title, repo, branch. The title is the user's label, else the terminal title, else the canonical label, and a tab or line break in it prints as a space. Repo and branch come from the first member, else `-`. With `--json` it prints one array of objects with `id`, `label` (that same title), `status`, `status_since`, `background`, `agent`, `repo` and `branch`; the last three are `null` where the text shows `shell` or `-`.

`rustling-tulipd read <session id> [--scrollback] [--json]` prints the session's screen as the daemon's grid has it, one row per line, at the daemon's current size. With `--json` it prints `{id, cols, rows, title, lines}`. It refuses at once against a daemon whose `Welcome.features` lack `screen_dump`.

With `--scrollback` it prints the session's scrollback as UTF-8 text: a cursor-forward sequence prints as spaces (at most 512 each), a cursor-position or next-line sequence as one line break, `\r\n` and a lone `\r` as `\n`, and every other escape sequence is dropped. Bytes that are not valid UTF-8 print as U+FFFD. With `--json` it prints `{id, truncated, text}`. It closes the connection as soon as the reply arrives, since that request also starts streaming the session's output to this connection.

Exit codes: 0 ok, 1 any failure, 2 usage, 5 no daemon running (`daemon.json` is missing, or no connection opens on the port it names, as after a crash). A listener that does not prove it holds the token is exit 1. A failure prints one stderr line `rustling-tulipd <verb>: <error>` and nothing on stdout. With `--json` it prints `{"error":{"code":"<code>","message":"<text>"}}` on stdout instead, the code being the daemon's (`not_found`, `no_screen`) when it sent one, else `failed`, `usage` or `no_daemon`.

## How a client starts and retires the daemon

The native client auto-spawns the daemon on first connect via `daemon_client::ensure_running` (`crates/daemon-client`), passing the protocol versions it speaks. The spawn breaks away from the caller's job; when that job forbids breakaway it launches the daemon through WMI `Win32_Process.Create` with the caller's full environment, outside the job (`wmi_launch.rs`, bounded at 30 s), and only if that fails starts it inside the job, logging which path ran. The daemon writes `port` + `auth_token` + `pid` + `supported_versions` + `build_version` to `daemon.json` in the config dir below; clients read that to connect.

A daemon sharing none of the client's versions is retired and replaced. The native client speaks only the current version, and the daemon's `supported` (`protocol-version.json`) lists only the current version too.

A daemon whose `build_version` is older than the client's own (a missing one counts as older) is retired and replaced too, with the reason `older build`; one whose `build_version` is newer is never retired, and `ensure_running` fails with `DaemonIsNewer` ("daemon is newer … update the client") instead.
