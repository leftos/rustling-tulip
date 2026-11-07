# Windows installer

`rustling-tulip.nsi` is a hand-written NSIS 3 script (Modern UI 2) that packages the three release binaries and the phone app's built files into a per-user setup exe. `.\rt.ps1 installer` builds the binaries and the phone app and runs `makensis`; it needs Node.js and pnpm (`.\rt.ps1 setup` installs them) and fails without pnpm or when the app's build fails. The three binaries and the setup exe are code-signed (see "Code signing"); the uninstaller NSIS writes at install time is not yet.

## What it installs

A per-user install, no elevation: every path and registry key is under the current user, so nothing asks for an administrator prompt.

| Installed | Where |
|---|---|
| `rustling-tulip-native.exe` (the GPUI client), `rustling-tulipd.exe` (the daemon), `rt-tracer.exe` (the per-session PTY supervisor) | `%LOCALAPPDATA%\Programs\rustling-tulip\` |
| `pwa\` (the phone app's built files, from `apps/pwa/dist`; the daemon's tailnet listener serves them) | `%LOCALAPPDATA%\Programs\rustling-tulip\` |
| Uninstaller | `%LOCALAPPDATA%\Programs\rustling-tulip\uninstall.exe` |
| Start menu shortcut | `%APPDATA%\Microsoft\Windows\Start Menu\Programs\rustling-tulip.lnk`; the installed client rewrites it at each start with the app id `leftos.RustlingTulip` and the toast activator CLSID `{354BE9DC-6D82-4269-8883-D13CE3E38648}`, so toast buttons pressed from Action Center reach it |
| Toast activator (written by the installed client, not the installer) | `HKCU\Software\Classes\CLSID\{354BE9DC-6D82-4269-8883-D13CE3E38648}\LocalServer32`, default `"<install dir>\rustling-tulip-native.exe" -ToastActivated` |
| Desktop shortcut (optional checkbox, off by default) | `%USERPROFILE%\Desktop\rustling-tulip.lnk` |
| Daemon autostart (optional checkbox, on by default) | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `rustling-tulip-daemon` = `"<install dir>\rustling-tulipd.exe" --detach` |
| Add/Remove Programs entry | `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\rustling-tulip` |
| `user-path.ps1` (the script that edits the user PATH) | `%LOCALAPPDATA%\Programs\rustling-tulip\` |
| Install directory on the user PATH | `HKCU\Environment`, value `Path`: the install directory is appended as the last entry unless it is already one (compared case-insensitively, ignoring a trailing `\`); a missing or empty value becomes just the install directory |

The PATH entry is there because an SSH client runs `rustling-tulipd bridge` on this host by name, and Windows sshd starts it with the user's PATH from the registry.

The installer runs the installed `user-path.ps1 -Add -Dir <install dir>` with Windows PowerShell (`powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass`), not NSIS string code, since an NSIS string holds 1024 characters and a user PATH can be longer. The script reads the value without expanding `%VAR%` entries, writes it back with the same kind (`REG_EXPAND_SZ` when it was that or missing, `REG_SZ` when it was `REG_SZ`), and broadcasts `WM_SETTINGCHANGE` ("Environment") so programs started next see it; a shell already open does not.

It prints one line, which lands in the install details: `Added <dir> to the user PATH.` or `<dir> is already on the user PATH.`. On failure it prints `Could not update the user PATH: <error>` and exits 1; the installer adds `The user PATH was not changed: user-path.ps1 -Add exited with <code>.` and carries on. A broadcast that fails after the write prints `The environment-change broadcast did not complete; programs started from Explorer may need a new sign-in to see the PATH.` and still exits 0.

A reinstall pointed at a different folder first runs `user-path.ps1 -Remove` for the previous install's folder (the uninstall key's `InstallLocation`, compared case-insensitively, ignoring a trailing `\`), so the user PATH never keeps a stale folder whose old `rustling-tulipd.exe` sshd would run.

The installer adds **no Windows Firewall rule**: the owner runs Fort Firewall, and the daemon listens on loopback (and the tailnet listener, when enabled) without a firewall exception.

The finish page offers "Run rustling-tulip", which starts the client unelevated.

## Staging and updates

Every install extracts its files, and the `pwa` folder (`File /r`; `makensis` fails without it), into `$INSTDIR\.staging` and runs the staged daemon's `rustling-tulipd apply-update --from <staging> --to $INSTDIR`, which stops the daemon and swaps the files in with rollback (`docs/auto-update.md`); the script removes `.staging` afterwards, and a non-zero exit shows `apply-update`'s message and aborts.

The app's `Restart to update` runs the downloaded setup as `/S /UPDATE /RELAUNCH`: `/UPDATE` writes only the uninstall key's `DisplayVersion`, leaving the autostart value, the desktop shortcut and the PATH entry as they are, and `/RELAUNCH` has `apply-update` start the client again afterwards.

## Stopping the daemon

After it copies the daemon and tracer and before it copies the client, and before the uninstaller deletes the files, the script stops a running daemon with the shipped daemon's own `rustling-tulipd shutdown`.

That command reads `daemon.json`, sends `/shutdown` with the bearer token only on a connection to a listener that has proven it holds that token (`GET /proof`), and prints one line (exit 0 stopped, nothing running or a stale file removed; 3 not proven, nothing sent; 1 anything else), which the script only logs.

Every `rt-tracer.exe` supervisor survives, so a reinstall over a client with sessions open leaves those sessions alive — the next daemon reattaches to them. The daemon and tracer run from a content-addressed binary cache, so the installed exes are never locked by a live session.

## What uninstall removes and keeps

Removed: the three exes, the `pwa` folder (and a leftover `pwa.old` or `pwa.new`), `user-path.ps1`, `uninstall.exe`, the install directory, both shortcuts, the `rustling-tulip-daemon` autostart value, the toast activator's `HKCU\Software\Classes\CLSID\{354BE9DC-…}` key, the uninstall registry key and the scheduled task `\leftos\rustling-tulip\daemon-{user SID}` that `rustling-tulipd bridge` may have registered (deleted right after the daemon is stopped; none is not an error).

Before it deletes the files, the uninstaller runs `user-path.ps1 -Remove`, which removes every user PATH entry naming the install directory (same comparison), leaves every other entry and its order as they were, deletes the `Path` value if nothing is left, and broadcasts `WM_SETTINGCHANGE`. It prints `Removed <dir> from the user PATH.` or `<dir> is not on the user PATH.`; a failure is logged the same way as on install and the uninstall carries on.

Kept: the config and data directories — `%APPDATA%\leftos\rustling-tulip\` (repos, workspaces, session history, secrets index) and `%LOCALAPPDATA%\leftos\rustling-tulip\` (worktrees, binary cache, tailnet certificate). Uninstalling never deletes the user's repos or session history; delete those folders by hand to start clean.

## Building

```powershell
.\rt.ps1 installer
```

The verb always builds the release profile (`cargo build --release -p daemon -p tracer -p rustling-tulip-native`) and writes `target\installer\rustling-tulip-<version>-setup.exe`, printing its path, and writes `rustling-tulip-<version>-setup.exe.sha256` (`<hex>  <file name>`) beside it, which the app's updater checks a download against. The version comes from `[workspace.package] version` in `Cargo.toml`; a pre-release suffix such as `0.1.0-alpha` stays in the exe's name and in `DisplayVersion`, while the four-part file-version resource uses the numeric part (`0.1.0.0`).

Requires NSIS 3, which the verb looks for at `%ProgramFiles(x86)%\NSIS\makensis.exe` and then on `PATH`:

```powershell
winget install NSIS.NSIS
```

`makensis` runs with `/WX`, so any NSIS warning fails the build.

## Code signing

The verb signs `rustling-tulipd.exe`, `rt-tracer.exe` and `rustling-tulip-native.exe` before NSIS packs them, and the setup exe before its `.sha256` is written, with Azure Artifact Signing (the Windows SDK SignTool with the Artifact Signing dlib). Every signature is timestamped (`http://timestamp.acs.microsoft.com`), since the Public Trust certificates last about three days, and each file is verified afterwards (`Get-AuthenticodeSignature`: Valid and timestamped). The uninstaller NSIS writes at install time stays unsigned for now.

Prerequisites:
- the Windows SDK SignTool;
- the Artifact Signing Client Tools: `winget install -e --id Microsoft.Azure.ArtifactSigningClientTools`;
- the Azure CLI: `winget install -e --id Microsoft.AzureCLI`, signed in with `az login` as an account that holds the Signer role on the certificate profile;
- Google Drive mounted as `G:`, holding the signing metadata file in the owner's private config on `G:`.

Signing refuses on a routed builder job. `.\rt.ps1 installer -Unsigned` builds without signing, for a test build on a PC not signed in to Azure; the other verbs reject `-Unsigned`. An unsigned build has the same file name as a signed one, so check before publishing:

```powershell
Get-AuthenticodeSignature target\installer\rustling-tulip-<version>-setup.exe
```

`Status` must be `Valid` with a `TimeStamperCertificate`.

## PATH test

The PATH add and remove logic lives in `user-path.ps1` as string functions (`Get-PathWith`, `Get-PathWithout`) that touch no registry. `user-path.tests.ps1` dot-sources them (dot-sourced, the script defines its functions and does nothing else), runs each case, including a PATH of more than 2500 characters, prints one `PASS` or `FAIL` line per case and a final `<n> failed` line, and exits with the failure count.

It also checks the installer's `-Dir` quoting: it builds the command line the `RT_USER_PATH` macro in `rustling-tulip.nsi` produces (a trailing `\` doubled, so a drive root such as `D:\` never forms `\"` with the closing quote) for `D:\`, `C:\Program Files\x\` and `C:\a'b$c\`, runs it under the 32-bit Windows PowerShell against a stub script in the temp folder that only echoes `-Dir`, and expects each folder back unchanged. The test copies the macro's quoting rule, so a change to one needs the same change in the other. Run it with Windows PowerShell, which the installer uses, from the repo root:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File tools\installer\user-path.tests.ps1
```

Never run `user-path.ps1 -Add` or `-Remove` by hand on a machine you care about: they edit your real user PATH.

## App icon

`icon.ico` is the app icon for the installer, the uninstaller and the client exe: `apps/native/build.rs` embeds it as icon resource 1 (`apps/native/app.rc`), which GPUI loads for the window and taskbar and which Explorer, the shortcuts and the Add/Remove Programs entry show.

It is built from the SVGs in `icon/`: `icon.svg` for 24 px and up, and `icon-16.svg`, a glyph drawn for 16 px. After changing either, rebuild it from the repo root with `uv run --script tools/installer/icon/build_icon.py` and commit the `.ico`. The design and the concepts it was picked from are in `docs/plans/canvases/app-icon/`.

## macOS app and disk image

`pack-macos.ps1` builds the macOS app bundle `rustling-tulip.app` (Apple Silicon, arm64 only), a disk image holding it and the zip the app's updater downloads. It runs on a Mac with PowerShell 7 (`pwsh`), Rust, the Xcode command-line tools, Node and pnpm: it builds the phone app with pnpm and puts it in the bundle at `Contents/Resources/pwa`, where a Mac daemon looks for it; it refuses to run anywhere else. From another machine over SSH, copy or clone the tree to the Mac first and run it there:

```sh
pwsh -NoProfile -File tools/installer/pack-macos.ps1 [-OutDir <dir>] [-SkipBuild] [-Sign]
```

- `-OutDir <dir>`: where the staging folder and the assets go; default `target/installer` under the repo root.
- `-SkipBuild`: use the binaries already in `target/release` and the phone app already built in `apps/pwa/dist`, instead of running `cargo build --release -p rustling-tulip-native -p daemon -p tracer` and the phone app's pnpm install and build.
- `-Sign`: sign with the Developer ID, notarize and staple instead of ad-hoc signing ("macOS code signing" below).

The script stops at the first native command that fails. It assembles the bundle in a fresh `macos-staging/dmg-root/` under `-OutDir`:

| In the bundle | What |
|---|---|
| `Contents/MacOS/rustling-tulip-native` | the client, the bundle's executable |
| `Contents/MacOS/rustling-tulipd`, `Contents/MacOS/rt-tracer` | the daemon and the tracer, beside the client, where the client looks for them |
| `Contents/Resources/pwa` | the phone app's built files, from `apps/pwa/dist` (`index.html`, `assets/`, `sw.js`); the daemon's tailnet listener serves them |
| `Contents/Resources/icon.icns` | the app icon, built with `sips` and `iconutil` from `icon/icon-1024.png` |
| `Contents/Info.plist` | from `macos/Info.plist.template`: bundle id `dev.leftos.rustling-tulip`, minimum macOS 13.0, both version keys set to the numeric part of the workspace version (`0.9.0` for `0.9.0-alpha`) |

Without `-Sign` it ad-hoc signs (`codesign --force --sign -`) the three binaries and then the bundle. It writes into `-OutDir`, printing the three paths last:

- `rustling-tulip-<version>-macos-arm64.dmg`: volume `rustling-tulip` holding the app and an `Applications` link to drag it onto, for a first install;
- `rustling-tulip-<version>-macos-arm64.zip`: the app zipped with `ditto -c -k --keepParent`, the update payload;
- `rustling-tulip-<version>-macos-arm64.zip.sha256`: `<hex>  <zip file name>`, which the updater checks the zip against.

`<version>` is `[workspace.package] version` in `Cargo.toml`, pre-release suffix included. An ad-hoc signed app opened from a downloaded disk image is stopped by Gatekeeper; a release is built with `-Sign`.

`icon/icon-1024.png` is written by `icon/build_icon.py` beside `icon.ico`; rebuild and commit both after changing `icon.svg`.

## macOS code signing

A release's macOS assets are signed with the owner's Developer ID **Application** certificate and notarized with the App Store Connect notary API key that `leftos/yaat` already uses. There is no CI here, so signing follows yaat's local release route: it runs on the Mac mini builder, started from this PC. A bare Mach-O cannot be stapled, which is why the binaries ship in a `.app`; the app and the `.dmg` are both signed with the Application identity, and there is no Installer certificate (no `.pkg`).

From the repo root on this PC:

```powershell
pwsh -NoProfile -File tools/installer/pack-macos-signed.ps1
```

It reads the signing set into its own process environment, then starts one pinned heavy gate run on `builder@mac-mini` of `pwsh -NoProfile -File tools/installer/pack-macos.ps1 -Sign`, logging to `.tmp/pack-macos.log`; the gate brings the three assets back into `target/installer`. The variables cross to the Mac as bare `--env NAME` gate flags, so no value is on a command line, in a log or in a file in the repo.

`pack-macos.ps1 -Sign` needs these variables, yaat's names (its `release-macos.yml` secrets), and fails naming the first one missing:

| Variable | What | Read from |
|---|---|---|
| `MACOS_DEVID_APP_CERT_P12_BASE64` | the Developer ID Application certificate and key, base64 | `devid-application.p12` |
| `MACOS_DEVID_CERT_PASSWORD` | the `.p12` password | `p12-password.txt` |
| `MACOS_SIGN_APP_IDENTITY` | the identity name, `Developer ID Application: <name> (<team>)` | `SIGN_APP_IDENTITY` in `ids.env` |
| `MACOS_NOTARY_API_KEY_P8_BASE64` | the App Store Connect API key, base64 | `AuthKey_*.p8` |
| `MACOS_NOTARY_API_KEY_ID` | the key's id | `NOTARY_API_KEY_ID` in `ids.env` |
| `MACOS_NOTARY_API_ISSUER_ID` | the key's issuer id | `NOTARY_API_ISSUER_ID` in `ids.env` |

The files live in the owner's Private Config on Google Drive, `G:\My Drive\Private Config\yaat\secrets\macos-signing\`, shared with yaat; `pack-macos-signed.ps1` stops naming the folder or the file when one is missing.

With `-Sign` the script, after assembling the bundle:

1. decodes the `.p12` into a temporary keychain under the Mac's temp folder (`security create-keychain`, `import`, `set-key-partition-list`) and adds it beside the user's keychain search list;
2. signs inside out with the hardened runtime and a timestamp (`codesign --force --options runtime --timestamp`): `rt-tracer`, `rustling-tulipd`, `rustling-tulip-native`, then the `.app` without `--deep`; no entitlements file;
3. zips the app with `ditto -c -k --keepParent`, notarizes it with `xcrun notarytool submit --wait` and the API key, and staples the app (`xcrun stapler staple`, up to three tries a minute apart, since a fresh ticket can take a moment to propagate);
4. writes the update zip and its `.sha256` from the stapled app, builds the `.dmg` around it, signs the `.dmg`, notarizes and staples it;
5. checks, failing on any refusal: `codesign --verify --deep --strict --verbose=2` on the app, `spctl --assess -vvv --type execute` on the app (it must say `accepted` and `source=Notarized Developer ID`), `spctl --assess -vvv --type open --context context:primary-signature` on the `.dmg` (`accepted`), and `xcrun stapler validate` on both.

Whatever happens, the user's keychain search list is put back and the temporary keychain, the decoded `.p12` and `.p8` and the notarization zip are deleted. A notarization Apple does not accept prints Apple's log for the submission and fails the run.

`notarytool` answering 403 about a required agreement is an Apple account matter, not a fault here: the Account Holder accepts the new agreement in App Store Connect (Agreements, Tax, and Banking), then the run is repeated (yaat's `docs/macos-code-signing.md`).

To check a downloaded asset on a Mac:

```sh
spctl --assess -vvv --type open --context context:primary-signature rustling-tulip-<version>-macos-arm64.dmg
xcrun stapler validate rustling-tulip-<version>-macos-arm64.dmg
```

## Hand test

1. Run the setup exe with the default options. It installs under `%LOCALAPPDATA%\Programs\rustling-tulip`, adds the Start menu entry and the autostart value, appends the install directory to the user PATH once (a second install adds no duplicate), and offers to run the client; it connects to a daemon. A new terminal finds `rustling-tulipd` by name.
2. Sign out and back in: the daemon starts at login (`%APPDATA%\leftos\rustling-tulip\config\logs\autostart.log` shows the launch) and the client connects without starting one itself.
3. With the client open and a session running, run the same setup exe again: it completes and the session survives (the tracer kept the PTY alive and the new daemon reattached).
4. Toasts: with the installed client connected to this PC's daemon, a device pairing request raises a toast under rustling-tulip (the sender name and icon, not PowerShell). Close the client, then press Approve… or Deny from Action Center: the client starts (`native.log`: "started by a toast press") and applies the press. `native.log` also says "toast activator registered", and the Start menu shortcut and the CLSID key above exist.
5. Update: install an older setup, open the client with a session running, then run a newer setup exe over it (or, with an older client connected to a published newer release, wait for the check or press Settings → General → Updates → Check now). The footer shows `Update {version} ready`; press `Restart to update`.

   The client closes, the new one opens on the new version with the toast `Updated to {version}`, the session is still running, `daemon.json` has the new `build_version`, no `.staging` folder or `.old` file is left in the install folder after the next launch, and the Run value, PATH entry and desktop shortcut are as they were.
6. Failed update: with a file in the install folder held open (for example `rustling-tulipd.exe bridge` running from it), press `Restart to update`. The client comes back on the old version with the toast `Update to {version} didn't install` whose detail names the running exe, nothing in the install folder has changed, and `Restart to update` is offered again.
7. Uninstall from Settings → Apps (or the Start menu): the `rustling-tulip-daemon` value is gone from `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, the `HKCU\Software\Classes\CLSID\{354BE9DC-…}` key is gone, the Start menu entry is gone, the install directory is gone and no longer on the user PATH (the other PATH entries unchanged), and `%APPDATA%\leftos\rustling-tulip\config\` is still there.
