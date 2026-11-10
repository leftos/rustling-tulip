# Auto-update

The native client finds a newer published release, downloads it in the background, verifies it, and on the user's word installs it and comes back up on the new version. The client and the daemon are both updated. Interactive sessions survive, because their tracers outlive the daemon.

The code is `crates/update-core` (feed, pick, download, verify, `last-result.json` read, the update lock; the client's `update.rs` re-exports it and keeps `Updater`, the start of the installer and the launch cleanup), the client's `update_view.rs` (timer, notice, Restart to update, toasts), `whats_new_view.rs` (the What's new window), the daemon's `cli_apply_update.rs` (the file swap), `crates/daemon-client/src/supervisor.rs` (daemon takeover) and the NSIS script `tools/installer/rustling-tulip.nsi`.

The client's side of the UI is in [native-client.md](./native-client.md) ("Updates"), the release steps in [release.md](./release.md) and what the installer does in [tools/installer/README.md](../tools/installer/README.md).

## Trust model

Updates come only from the repo's GitHub Releases (`leftos/rustling-tulip`, public) over HTTPS. A download is installed only when its SHA-256 matches the `.sha256` asset of the same release.

There is no owner-held signing key. The installer and the binaries in it carry an Authenticode signature from Azure Artifact Signing (`tools/installer/README.md` "Code signing"), but the updater does not check it: the hash is the only check on a download.

In scope:
- A network attacker, stopped by TLS to GitHub plus the hash check.
- A corrupted or partial download.
- Running a file other than the one verified.
- Accidental downgrade.
- Two installers at once.
- A half-applied update.

Out of scope:
- A compromised GitHub account or release host, which could publish a matching hash for a malicious installer, hold back releases, or serve an old one as newest.
- A process running as the same user, which can already write the install folder, an ordinary profile's binary cache and HKCU Run. An elevated daemon's binary cache is under its protected root, which that process cannot write (`docs/on-disk.md`, "An elevated daemon's protected profile").

Velopack was considered and not used. It would replace the NSIS installer and add the .NET `vpk` packaging tool to the release, where the existing per-user NSIS installer needs only the staging step below.

## Decisions

1. **Feed.** `GET https://api.github.com/repos/leftos/rustling-tulip/releases?per_page=100` (unauthenticated, `User-Agent: rustling-tulip/<version>`). Every release so far is a pre-release, so `/releases/latest` would skip them. Take the newest non-draft release by semver, pre-releases included, that has both this platform's asset and its `.sha256` (`update::asset_name`): `rustling-tulip-<version>-setup.exe` on Windows, `rustling-tulip-<version>-macos-arm64.zip` on Apple-silicon macOS. A release missing this platform's pair is skipped for that platform only. A platform the feed publishes no asset for (Intel macOS, Linux) is offered nothing. `RUSTLING_TULIP_UPDATE_FEED` overrides the base URL for tests.
2. **Version.** The client's version is `env!("CARGO_PKG_VERSION")`. An update is offered only when the feed's version is strictly newer by semver; there is no in-app downgrade. A dev build (`debug_assertions`) checks only when `RUSTLING_TULIP_UPDATE_FEED` is set.
3. **When it checks.** At every launch, whenever the last check ran (nothing records its time), then every hour; after a failed automatic check it retries after 5 minutes, then 15, then every hour until one succeeds (`update_view.rs` `next_check`) while the client runs; a check is skipped while an update is ready. When a newer version is found, it is downloaded in the background. Settings → General → Updates has a "Check for updates" toggle (`native-ui.json` `general.check_updates`, default on); off means no automatic check and no download, and the page's "Check now" button, which checks either way, is the only way to check.
4. **Download and verify.**
   - The download goes to a fresh folder `%LOCALAPPDATA%\leftos\rustling-tulip\data\updates\<version>-<random>\` (`RUSTLING_TULIP_UPDATES_DIR` overrides the `updates` root; a non-empty value is used as given, an empty one counts as unset), which holds only the installer (on macOS, only the zip).
   - The client parses the `.sha256` asset as `<64 hex>  <file name>` and refuses it unless the name matches.
   - It hashes the download while holding the file open with no write or delete sharing, and keeps that handle open until the installer process has started, so the verified file is the file that runs.
   - On a hash mismatch it deletes the folder, logs `update: the download's SHA-256 does not match its release; discarded`, and retries at the next check.
   - Older `updates\*` folders are deleted at launch, on every platform.
5. **Notice.** Once a verified installer is ready, the footer shows `Update {version} ready` with a `Restart to update` button (selector `footer-update`). Nothing installs without that press. The Settings page shows the current version, the last check's time and result, and the ready update, with the same `Restart to update` button beside `Check now` (selector `settings-restart-to-update`), so a manual check installs without closing Settings. While an update is ready both places also have a `What's new` button (`footer-whats-new`, `settings-whats-new`) that opens the What's new window on the ready update's notes (decision 13).
6. **Restart to update.**
   - The client takes the named mutex `Local\rustling-tulip-update`. Another holder means an update is already running, so it refuses with the toast `An update is already running`.
   - It starts the installer detached, outside its job, as `setup.exe /S /UPDATE /RELAUNCH`, then quits by the Keep-sessions-running path, which leaves the daemon and tracers up.
   - On macOS there is no installer or mutex. `update::start_macos_update` refuses a translocated bundle, or one whose folder it cannot write a probe file in, with `rustling-tulip.app runs from a place the update cannot replace; move it to Applications and update again; nothing was changed`. It unpacks the verified zip with `ditto -x -k` into a hidden `.rustling-tulip-update-<version>-<8 hex>/` folder beside the installed `rustling-tulip.app` (same volume), under a 120 s bound, and checks the folder holds `rustling-tulip.app/Contents/MacOS/rustling-tulipd`. It then starts that daemon's `apply-update --from <staged app> --to <installed app> --old-version <client version> --update --relaunch` in its own process group, and quits at once.
   - The other macOS start failures are `the update archive holds no rustling-tulip.app`, `rustling-tulip-native is not running from a rustling-tulip.app bundle; update by installing the new release` and `<program> did not finish within 120 s`. A failed start removes its staging folder and changes nothing else.
   - The macOS unpack runs on the UI thread, so the window pauses for a few seconds.
   - Off Windows there is no cross-process update lock: one client quits right after it starts an update, and two clients updating at once are not excluded.
   - No other path starts the installer.
7. **Applying the files: `rustling-tulipd apply-update`.** Every install, update or not, extracts its files into `$INSTDIR\.staging\` and then runs the staged daemon's `apply-update --from <staging> --to $INSTDIR [--old-version <v>] [--update] [--relaunch] [--restart-daemon]` (`crates/daemon/src/cli_apply_update.rs`). The steps live in Rust so they can be tested without running an installer.

   NSIS passes `--old-version` from the uninstall key's `DisplayVersion` and runs no exe from the install folder to learn it.

   `apply-update`:
   - Takes the mutex `Local\rustling-tulip-update`, exiting 4 if it is held.
   - Waits up to 30 s for `rustling-tulip-native.exe` processes running from `$INSTDIR` to exit.
   - Stops the daemon first, before writing any file, and requires success (exit 2 on failure, nothing changed).
   - Exits 3 with `<exe> is still running from the install folder (pid <n>); nothing was changed` when any process still runs an exe from `$INSTDIR` (a `bridge` for an SSH client, `--detach`, `pair`, `screen`).
   - Copies each file as `<name>.new`, then renames the old one to `<name>.old` and the new one into place, client last; on any failure it puts every file back (exit 5).
   - Moves the staged `pwa` folder (the phone app) into place by rename before the client, inside the same rollback: staged `pwa` becomes `pwa.new`, the installed `pwa` becomes `pwa.old`, then `pwa.new` becomes `pwa`; a leftover `pwa.old` or `pwa.new` is removed first, and exit 5 puts the old folder back. A staging without `pwa` leaves the installed one alone, and any other staged folder is ignored. After success `pwa.old` is removed; a failure to remove it is reported and never fails the update.
   - Writes `updates\last-result.json` (`{from, message, ok, to}`) in every outcome, then with `--relaunch` starts the client once with `--updated-from <old version>`, on success and on failure. With `--restart-daemon` (the installer's `/RESTARTDAEMON`, which a host's daemon passes when a remote client asked it to update) it relaunches the client the same way when one was running from the install folder at its start, and otherwise, or when that client fails to start, starts the installed daemon with `--detach`; never both. The early refusal while another update holds the update mutex starts nothing.

   Exit codes: 0 applied, 1 a bad command line or unresolvable folders, 2 the daemon would not stop, 3 an exe in the install folder is still running, 4 another update holds the mutex, 5 a file failed and every file was put back. `rustling-tulipd --version` prints the daemon's version.

   On macOS `--to` is the installed `<name>.app` and `--from` a staged `.app`, and the swap replaces the whole bundle by rename: the old bundle moves to `<name>.app.old`, the new one is renamed into place, a failure puts the old app back, and the old copy is removed after success. The exit codes are the same. Exit 5 comes before the daemon stops when the bundle is translocated or its folder is not writable (`nothing was changed`). The client wait, the in-use refusal and `--relaunch` look at `<to>/Contents/MacOS`. The staged bundle must be on the same volume as the installed one; the macOS client stages it and starts `apply-update` as decision 6 describes.

   NSIS removes `.staging` afterwards. Under `/UPDATE` it writes only the uninstall key's `DisplayVersion`, after exit 0, leaving the Run value, the desktop shortcut and PATH as they are. A normal install writes them as before, and on a non-zero exit shows `apply-update`'s message and aborts.
8. **After the restart.**
   - The client reads `last-result.json` once and deletes it, and drops the `--updated-from <v>` launch argument. Success shows the sticky toast `Updated to {version}` with a `What's new` action (decision 13). Failure shows `Update to {version} didn't install` with the recorded message as its detail, and the ready update stays offered.
   - `.old` files are deleted at the next launch. On macOS the launch also deletes `rustling-tulip.app.old` and every `.rustling-tulip-update-*` folder beside the running bundle (`update::clean_at_launch`).
9. **Daemon takeover.**
   - `daemon.json` gains `build_version` (the daemon's `CARGO_PKG_VERSION`).
   - `daemon_client::ensure_running` retires a protocol-compatible daemon whose `build_version` is older than the client's own (reason `older build`).
   - `ensure_running` never retires a daemon whose `build_version` is newer. An older client facing a newer daemon reports `daemon is newer ({daemon}) than this client ({client}); update the client` and does not start its own.
   - A missing version counts as older.
10. **Sessions.** Tracers keep running across the daemon stop, and the new daemon reattaches them as after any daemon restart. The cache keeps tracer copies that sidecars name, so live sessions keep their hook shim. An update across a breaking tracer-ABI or sidecar change does not promise session survival, and the release notes say so when a release breaks it. The release that gave an elevated daemon its own profile notes that an administrator's sessions start with an empty profile while the ordinary one is untouched. Saved user data always survives an update (CLAUDE.md, "Saved user data survives every update").
11. **Remote update.** A paired remote client may ask a Windows host's daemon to update itself: `ClientMessage::RemoteUpdate`, offered when `Welcome.features` lists `remote_update`, which names no file, version or path (`crates/daemon/src/server/update.rs`, `docs/plans/remote-update.md`).
    - **Checks.** The daemon refuses with an `Error` echoing the `request_id`, the first that applies: not Windows, a development build (`update_core::checks_allowed`), an elevated daemon, no installed copy (the uninstall key's `InstallLocation`, holding the daemon), no updates folder, a daemon that does not run from that install, another update running (`busy`), any process other than the app running from the install folder (the app's exe does not count; the countdown below handles it). Each refusal is `update_refused`, except `busy`.
    - **Update.** On a thread of its own, the daemon takes the update mutex before it downloads, clears old downloads, downloads and verifies the same release asset the app would (`update_core::check_and_download`), and then launches the installer detached; the mutex and the in-daemon guard are held through the countdown and until after the launch, the verified file open until the installer has started. A host already on the newest release, a failed download and an installer that does not start are `update_refused`. It answers `RemoteUpdateStarted { request_id, version }`, or an `Error` (`update_refused`, `busy`, or `update_canceled` when a local connection cancels the countdown, or `update_refused` when the host's daemon stops during it). While it runs the idle-exit watcher keeps the daemon up.
    - **Countdown.** After the download is verified, a host with any local connection attached (the app, an SSH bridge, a CLI verb) counts down 10 s, telling its local connections (`HostUpdateScheduled`, then `HostUpdateLaunched` or `HostUpdateCanceled`; `docs/wire-protocol.md`). A local connection can cancel with `CancelHostUpdate`, which ends the update with `update_canceled` for the requester.

      At the end the daemon looks for the app running from the install folder, sends Launched, waits up to 30 s for the app to leave, then starts the installer with `/S /UPDATE /RELAUNCH /RESTARTDAEMON` when the app was running, else `/S /UPDATE /RESTARTDAEMON`.

      With no local connection it launches at once. The native client shows the countdown only for This PC's own daemon (`docs/native-client.md`). The idle-exit watcher stays held off through it.
    - **Who.** Any connection kind, a tailnet one included (`host_only_refusal` does not refuse it). Nothing else triggers it: the app's updater is unchanged, and no SSH host is served yet. Cancelling is local-only.
12. **Release procedure.** After the installer is built, `rt.ps1 installer` writes `target/installer/rustling-tulip-<version>-setup.exe.sha256` (`<hex>  <file name>`), and the publish step attaches both files ([release.md](./release.md)).
13. **What's new.** A modal window shows the changelog of an update, from the footer or Settings button on a ready update, or from the action of the installed toast (a manual install over an older saved version raises that toast too, and it opens in the main window from a pop-out).
    - **Source.** Each GitHub release's body, which is that version's `CHANGELOG.md` section without its `## v` heading ([release.md](./release.md)). The feed asks for 100 releases a page. The window covers every release after the start version up to and including the new one, newest first, one `## v<version>` heading each. Drafts, tags that are not `v<semver>` and empty notes are left out; a release without installer assets is included.
    - **Start of the range.** For an update: the last-installed version when it is older than the new one, else the replaced version the installer recorded (`last-result.json` `from`), else the new release alone. For a manual install: the last-installed version.
    - **Last-installed version.** `native-ui.json` `last_installed_version`, the client's version, written at launch only by a build whose updater is active: not a dev build without `RUSTLING_TULIP_UPDATE_FEED`, not the cloaked smoke window. A file without the field loads as unknown, and nothing migrates.
    - **States and timeout.** Loading, empty (`No release notes for this version.`) and failure (the error and an `Open the releases page` button); the fetch times out after 30 s. The window is in [native-client.md](./native-client.md) ("Updates").

## Proof

The proof is the local-server tests in `crates/update-core` (feed parse, pick, download, hash, `.sha256` parse, against a local HTTP server on the real clock), the `ui_update` specs (`apps/native/tests/ui/ui_update.rs`, with the updater injected), the macOS start and cleanup tests in `apps/native/src/update.rs` (argument list, bundle path, leftover removal, and on a Mac the refusals and staging), the `apply-update` and supervisor tests, and the release's agent-run smoke of the installer.

There is no live e2e spec and no fake-releases feed shim. `.\rt.ps1 native-shot update` shows the footer notice and `native-shot update-notes` the What's new window.
