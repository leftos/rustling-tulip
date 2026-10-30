# Auto-update (RT-582)

The native client finds a newer published release, downloads it in the background, verifies it, and on the user's word installs it and comes back up on the new version. The client and the daemon are both updated. Interactive sessions survive, because their tracers outlive the daemon.

Inputs: the RT-582 map (site inventory of the installer, release steps, binary cache, supervisor) and an invariant pass (actors, invariants I1 to I13, windows W1 to W11), both summarised here.

## Trust model

Updates come only from the repo's GitHub Releases (`leftos/rustling-tulip`, public) over HTTPS. A download is installed only when its SHA-256 matches the `.sha256` asset of the same release.

This is the yaat model (Velopack's `GithubSource`, `X:\dev\yaat\src\Yaat.Client.Core\Services\UpdateService.cs:59`): no owner-held signing key and no code-signing certificate. The owner ruled that out as unnecessary.

In scope:
- A network attacker, stopped by TLS to GitHub plus the hash check.
- A corrupted or partial download.
- Running a file other than the one verified (W1).
- Accidental downgrade.
- Two installers at once.
- A half-applied update.

Out of scope:
- A compromised GitHub account or release host, which could publish a matching hash for a malicious installer, hold back releases, or serve an old one as newest.
- A process running as the same user, which can already write the install folder, the binary cache and HKCU Run.

Velopack was considered and not used. It would replace the NSIS installer and add the .NET `vpk` packaging tool to the release, where the existing per-user NSIS installer needs only the fixes below.

## Decisions

1. **Feed.** `GET https://api.github.com/repos/leftos/rustling-tulip/releases` (unauthenticated, `User-Agent: rustling-tulip/<version>`). Every release so far is a pre-release, so `/releases/latest` would skip them. Take the newest non-draft release by semver, pre-releases included, that has both `rustling-tulip-<version>-setup.exe` and `rustling-tulip-<version>-setup.exe.sha256`. A release missing either asset is skipped. `RUSTLING_TULIP_UPDATE_FEED` overrides the base URL for tests.
2. **Version.** The client's version is `env!("CARGO_PKG_VERSION")`. An update is offered only when the feed's version is strictly newer by semver (I2); there is no in-app downgrade. A dev build (`debug_assertions`) never checks.
3. **When.** Check 30 s after launch, then every 24 h while the client runs. When a newer version is found, download it in the background. Settings → General gets a "Check for updates" toggle (`native-ui.json` `general.check_updates`, default on); off means no check, no download, and the Settings page's "Check now" button is the only way to check.
4. **Download and verify (I3, W1).**
   - The download goes to a fresh folder `%LOCALAPPDATA%\leftos\rustling-tulip\data\updates\<version>-<random>\`, which holds only the installer.
   - The client parses the `.sha256` asset as `<64 hex>  <file name>` and refuses it unless the name matches.
   - It hashes the download while holding the file open with no write or delete sharing, and keeps that handle open until the installer process has started, so the verified file is the file that runs.
   - On a hash mismatch it deletes the folder, logs `update: the download's SHA-256 does not match its release; discarded`, and retries at the next check.
   - Older `updates\*` folders are deleted at launch.
5. **Notice.** Once a verified installer is ready, the footer shows `Update {version} ready` with a `Restart to update` button (selector `footer-update`). Nothing installs without that press. The Settings page shows the current version, the last check's time and result, and the ready update.
6. **Restart to update (I4, I6, I13).**
   - The client takes the named mutex `Local\rustling-tulip-update`. Another client that holds it means an update is already running, so it refuses with a toast.
   - The client starts the installer detached, outside its job, as `setup.exe /S /UPDATE /RELAUNCH`, then quits by the Keep-sessions-running path, which leaves the daemon and tracers up.
   - No other path starts the installer.
7. **Installer, in update mode (`/UPDATE`)** (I4 to I8, I11, W3, W4, W8, W11):
   - It takes the same mutex `Local\rustling-tulip-update`, and also takes it on a normal install, so two installers never run at once (W7).
   - It waits up to 30 s for `rustling-tulip-native.exe` processes running from `$INSTDIR` to exit.
   - It stops the daemon first, before writing any file, with `rustling-tulipd shutdown`, and requires success. On failure it writes no file, records the failure, and relaunches the old client.
   - If any process is still running an exe from `$INSTDIR` (a `bridge` for an SSH client, `--detach`, `pair`, `screen`), it writes no file, records `update: <exe> is still running from the install folder (pid <n>); nothing was changed`, and relaunches the old client.
   - It copies each exe as `<name>.new`, then renames the old one to `<name>.old` and the new one into place, client last. On any failure it renames the `.old` files back.
   - It leaves the Run value, the desktop shortcut and the install folder as they are (I11). Only a normal install writes them.
   - It writes `updates\last-result.json` (`{from, to, ok, message}`), then starts the client once with `--updated-from <old version>` (`/RELAUNCH`).
   - A normal, non-update install also stops the daemon before writing, and stops with a message when the client is running rather than prompting Abort/Retry/Ignore.
8. **After the restart.**
   - The client reads `last-result.json` once and deletes it. Success shows the toast `Updated to {version}`. Failure shows `Update to {version} didn't install` with the recorded message as its detail, and the ready update stays offered.
   - `.old` files are deleted at the next launch.
9. **Daemon takeover (I8, I9, W6).**
   - `daemon.json` gains `build_version` (the daemon's `CARGO_PKG_VERSION`).
   - `daemon_client::ensure_running` retires a protocol-compatible daemon whose `build_version` is older than the client's own.
   - `ensure_running` never retires a daemon whose `build_version` is newer. An older client facing a newer daemon reports `daemon is newer ({daemon}) than this client ({client}); update the client` and does not start its own.
   - Missing versions count as older.
10. **Sessions (I10).** Tracers keep running across the daemon stop, and the new daemon reattaches them as after any daemon restart. The cache keeps tracer copies that sidecars name, so live sessions keep their hook shim (W10). An update across a breaking tracer-ABI or sidecar change does not promise session survival. Before the first release the app keeps no backward compatibility, and the release notes say so when a release breaks it.
11. **Only this PC (I12).** The updater lives in the native client only. No daemon message, tailnet peer or SSH host triggers or feeds it.
12. **Release procedure.** After step 5 builds the installer, step 5b writes `target/installer/rustling-tulip-<version>-setup.exe.sha256` (`<hex>  <file name>`, via `rt.ps1 installer`). Step 8 attaches both files.

## Steps

- [ ] U.1 Release side: `rt.ps1 installer` writes the `.sha256`; `docs/release.md` steps 5 and 8; `daemon.json` `build_version`; supervisor retire-if-older and never-retire-newer (decision 9), with tests.
- [ ] U.2 Updater core, a plain-Rust module in the client: feed parse, semver pick, download into a fresh folder, hash with a held handle, `.sha256` parse, cleanup. Tests against a local HTTP server on the real clock, plus unit tests for parse and pick.
- [ ] U.3 Installer update mode (decision 7): the mutex, the wait, stop-first, the in-use check, `.new`/`.old` renames with rollback, `last-result.json`, relaunch, the I11 preservation, and the normal-install fixes. Plus `tools/installer/README.md` hand tests.
- [ ] U.4 Client UI and lifecycle: timer, footer notice, Restart to update (mutex, detached start, keep-sessions quit), Settings toggle, Check now and version line, the after-restart toast. Specs in `tests/ui/ui_update.rs`, with the updater injected.
- [ ] U.5 Live proof: a `tools/e2e/fake-releases` feed shim and a stand-in installer in an isolated `.tmp` dir, an e2e spec for check, download, verify, restart and relaunch, then docs (CLAUDE.md's deferred entry and env table, `docs/architecture.md`, `docs/native-client.md`).

## Booked separately

- `crates/daemon/src/tracer_client.rs:275-285` looks for the installed tracer under `%LOCALAPPDATA%\rustling-tulip\`, while the installer installs to `%LOCALAPPDATA%\Programs\rustling-tulip\` (`.nsi:44`).
- `crates/daemon/src/binary_cache.rs:62` and `crates/daemon-client/src/supervisor.rs:791` trust a cache entry by name without re-hashing (W2; same-user scope).
