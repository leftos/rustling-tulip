# Auto-update

The native client finds a newer published release, downloads it in the background, verifies it, and on the user's word installs it and comes back up on the new version. The client and the daemon are both updated. Interactive sessions survive, because their tracers outlive the daemon.

The code is the client's `update.rs` (feed, pick, download, verify), `update_view.rs` (timer, notice, Restart to update, toasts), the daemon's `cli_apply_update.rs` (the file swap), `crates/daemon-client/src/supervisor.rs` (daemon takeover) and the NSIS script `tools/installer/rustling-tulip.nsi`.

The client's side of the UI is in [native-client.md](./native-client.md) ("Updates"), the release steps in [release.md](./release.md) and the owner's hand tests in [tools/installer/README.md](../tools/installer/README.md).

## Trust model

Updates come only from the repo's GitHub Releases (`leftos/rustling-tulip`, public) over HTTPS. A download is installed only when its SHA-256 matches the `.sha256` asset of the same release.

There is no owner-held signing key and no code-signing certificate; the owner ruled that out as unnecessary.

In scope:
- A network attacker, stopped by TLS to GitHub plus the hash check.
- A corrupted or partial download.
- Running a file other than the one verified.
- Accidental downgrade.
- Two installers at once.
- A half-applied update.

Out of scope:
- A compromised GitHub account or release host, which could publish a matching hash for a malicious installer, hold back releases, or serve an old one as newest.
- A process running as the same user, which can already write the install folder, the binary cache and HKCU Run.

Velopack was considered and not used. It would replace the NSIS installer and add the .NET `vpk` packaging tool to the release, where the existing per-user NSIS installer needs only the staging step below.

## Decisions

1. **Feed.** `GET https://api.github.com/repos/leftos/rustling-tulip/releases` (unauthenticated, `User-Agent: rustling-tulip/<version>`). Every release so far is a pre-release, so `/releases/latest` would skip them. Take the newest non-draft release by semver, pre-releases included, that has both `rustling-tulip-<version>-setup.exe` and `rustling-tulip-<version>-setup.exe.sha256`. A release missing either asset is skipped. `RUSTLING_TULIP_UPDATE_FEED` overrides the base URL for tests.
2. **Version.** The client's version is `env!("CARGO_PKG_VERSION")`. An update is offered only when the feed's version is strictly newer by semver; there is no in-app downgrade. A dev build (`debug_assertions`) checks only when `RUSTLING_TULIP_UPDATE_FEED` is set.
3. **When it checks.** 30 s after launch, then every 24 h while the client runs; a check is skipped while an update is ready. When a newer version is found, it is downloaded in the background. Settings → General → Updates has a "Check for updates" toggle (`native-ui.json` `general.check_updates`, default on); off means no automatic check and no download, and the page's "Check now" button, which checks either way, is the only way to check.
4. **Download and verify.**
   - The download goes to a fresh folder `%LOCALAPPDATA%\leftos\rustling-tulip\data\updates\<version>-<random>\` (`RUSTLING_TULIP_UPDATES_DIR` overrides the `updates` root; a non-empty value is used as given, an empty one counts as unset), which holds only the installer.
   - The client parses the `.sha256` asset as `<64 hex>  <file name>` and refuses it unless the name matches.
   - It hashes the download while holding the file open with no write or delete sharing, and keeps that handle open until the installer process has started, so the verified file is the file that runs.
   - On a hash mismatch it deletes the folder, logs `update: the download's SHA-256 does not match its release; discarded`, and retries at the next check.
   - Older `updates\*` folders are deleted at launch.
5. **Notice.** Once a verified installer is ready, the footer shows `Update {version} ready` with a `Restart to update` button (selector `footer-update`). Nothing installs without that press. The Settings page shows the current version, the last check's time and result, and the ready update.
6. **Restart to update.**
   - The client takes the named mutex `Local\rustling-tulip-update`. Another holder means an update is already running, so it refuses with the toast `An update is already running`.
   - It starts the installer detached, outside its job, as `setup.exe /S /UPDATE /RELAUNCH`, then quits by the Keep-sessions-running path, which leaves the daemon and tracers up.
   - No other path starts the installer.
7. **Applying the files: `rustling-tulipd apply-update`.** Every install, update or not, extracts its files into `$INSTDIR\.staging\` and then runs the staged daemon's `apply-update --from <staging> --to $INSTDIR [--old-version <v>] [--update] [--relaunch]` (`crates/daemon/src/cli_apply_update.rs`). The steps live in Rust so they can be tested without running an installer.

   NSIS passes `--old-version` from the uninstall key's `DisplayVersion` and runs no exe from the install folder to learn it.

   `apply-update`:
   - Takes the mutex `Local\rustling-tulip-update`, exiting 4 if it is held.
   - Waits up to 30 s for `rustling-tulip-native.exe` processes running from `$INSTDIR` to exit.
   - Stops the daemon first, before writing any file, and requires success (exit 2 on failure, nothing changed).
   - Exits 3 with `<exe> is still running from the install folder (pid <n>); nothing was changed` when any process still runs an exe from `$INSTDIR` (a `bridge` for an SSH client, `--detach`, `pair`, `screen`).
   - Copies each file as `<name>.new`, then renames the old one to `<name>.old` and the new one into place, client last; on any failure it puts every file back (exit 5).
   - Writes `updates\last-result.json` (`{from, message, ok, to}`) in every outcome, then with `--relaunch` starts the client once with `--updated-from <old version>`, on success and on failure.

   Exit codes: 0 applied, 1 a bad command line or unresolvable folders, 2 the daemon would not stop, 3 an exe in the install folder is still running, 4 another update holds the mutex, 5 a file failed and every file was put back. `rustling-tulipd --version` prints the daemon's version.

   NSIS removes `.staging` afterwards. Under `/UPDATE` it writes only the uninstall key's `DisplayVersion`, after exit 0, leaving the Run value, the desktop shortcut and PATH as they are. A normal install writes them as before, and on a non-zero exit shows `apply-update`'s message and aborts.
8. **After the restart.**
   - The client reads `last-result.json` once and deletes it, and drops the `--updated-from <v>` launch argument. Success shows the toast `Updated to {version}`. Failure shows `Update to {version} didn't install` with the recorded message as its detail, and the ready update stays offered.
   - `.old` files are deleted at the next launch.
9. **Daemon takeover.**
   - `daemon.json` gains `build_version` (the daemon's `CARGO_PKG_VERSION`).
   - `daemon_client::ensure_running` retires a protocol-compatible daemon whose `build_version` is older than the client's own (reason `older build`).
   - `ensure_running` never retires a daemon whose `build_version` is newer. An older client facing a newer daemon reports `daemon is newer ({daemon}) than this client ({client}); update the client` and does not start its own.
   - A missing version counts as older.
10. **Sessions.** Tracers keep running across the daemon stop, and the new daemon reattaches them as after any daemon restart. The cache keeps tracer copies that sidecars name, so live sessions keep their hook shim. An update across a breaking tracer-ABI or sidecar change does not promise session survival, and the release notes say so when a release breaks it. Saved user data always survives an update (CLAUDE.md, "Saved user data survives every update").
11. **Only this PC.** The updater lives in the native client only. No daemon message, tailnet peer or SSH host triggers or feeds it.
12. **Release procedure.** After the installer is built, `rt.ps1 installer` writes `target/installer/rustling-tulip-<version>-setup.exe.sha256` (`<hex>  <file name>`), and the publish step attaches both files ([release.md](./release.md)).

## Proof

The proof is the local-server tests in `apps/native/src/update.rs` (feed parse, pick, download, hash, `.sha256` parse, cleanup, against a local HTTP server on the real clock), the `ui_update` specs (`apps/native/tests/ui/ui_update.rs`, with the updater injected), the `apply-update` and supervisor tests, the release's agent-run smoke of the installer, and the owner's hand test in `tools/installer/README.md`.

There is no live e2e spec and no fake-releases feed shim. `.\rt.ps1 native-shot update` shows the footer notice.
