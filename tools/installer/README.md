# Windows installer

`rustling-tulip.nsi` is a hand-written NSIS 3 script (Modern UI 2) that packages the three release binaries into a per-user setup exe. `.\rt.ps1 installer` builds the binaries and runs `makensis`; nothing is code-signed yet, so SmartScreen warns on first run ("Windows protected your PC" → More info → Run anyway).

## What it installs

A per-user install, no elevation: every path and registry key is under the current user, so nothing asks for an administrator prompt.

| Installed | Where |
|---|---|
| `rustling-tulip-native.exe` (the GPUI client), `rustling-tulipd.exe` (the daemon), `rt-tracer.exe` (the per-session PTY supervisor) | `%LOCALAPPDATA%\Programs\rustling-tulip\` |
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

## Stopping the daemon

After it copies the daemon and tracer and before it copies the client, and before the uninstaller deletes the files, the script stops a running daemon with the shipped daemon's own `rustling-tulipd shutdown`.

That command reads `daemon.json`, sends `/shutdown` with the bearer token only on a connection to a listener that has proven it holds that token (`GET /proof`), and prints one line (exit 0 stopped, nothing running or a stale file removed; 3 not proven, nothing sent; 1 anything else), which the script only logs.

Every `rt-tracer.exe` supervisor survives, so a reinstall over a client with sessions open leaves those sessions alive — the next daemon reattaches to them. The daemon and tracer run from a content-addressed binary cache, so the installed exes are never locked by a live session.

## What uninstall removes and keeps

Removed: the three exes, `user-path.ps1`, `uninstall.exe`, the install directory, both shortcuts, the `rustling-tulip-daemon` autostart value, the toast activator's `HKCU\Software\Classes\CLSID\{354BE9DC-…}` key and the uninstall registry key.

Before it deletes the files, the uninstaller runs `user-path.ps1 -Remove`, which removes every user PATH entry naming the install directory (same comparison), leaves every other entry and its order as they were, deletes the `Path` value if nothing is left, and broadcasts `WM_SETTINGCHANGE`. It prints `Removed <dir> from the user PATH.` or `<dir> is not on the user PATH.`; a failure is logged the same way as on install and the uninstall carries on.

Kept: the config and data directories — `%APPDATA%\leftos\rustling-tulip\` (repos, workspaces, session history, secrets index) and `%LOCALAPPDATA%\leftos\rustling-tulip\` (worktrees, binary cache, tailnet certificate). Uninstalling never deletes the user's repos or session history; delete those folders by hand to start clean.

## Building

```powershell
.\rt.ps1 installer
```

The verb always builds the release profile (`cargo build --release -p daemon -p tracer -p rustling-tulip-native`) and writes `target\installer\rustling-tulip-<version>-setup.exe`, printing its path. The version comes from `[workspace.package] version` in `Cargo.toml`; a pre-release suffix such as `0.1.0-alpha` stays in the exe's name and in `DisplayVersion`, while the four-part file-version resource uses the numeric part (`0.1.0.0`).

Requires NSIS 3, which the verb looks for at `%ProgramFiles(x86)%\NSIS\makensis.exe` and then on `PATH`:

```powershell
winget install NSIS.NSIS
```

`makensis` runs with `/WX`, so any NSIS warning fails the build.

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

## Hand test

1. Run the setup exe with the default options. It installs under `%LOCALAPPDATA%\Programs\rustling-tulip`, adds the Start menu entry and the autostart value, appends the install directory to the user PATH once (a second install adds no duplicate), and offers to run the client; it connects to a daemon. A new terminal finds `rustling-tulipd` by name.
2. Sign out and back in: the daemon starts at login (`%APPDATA%\leftos\rustling-tulip\config\logs\autostart.log` shows the launch) and the client connects without starting one itself.
3. With the client open and a session running, run the same setup exe again: it completes and the session survives (the tracer kept the PTY alive and the new daemon reattached).
4. Toasts: with the installed client connected to this PC's daemon, a device pairing request raises a toast under rustling-tulip (the sender name and icon, not PowerShell). Close the client, then press Approve… or Deny from Action Center: the client starts (`native.log`: "started by a toast press") and applies the press. `native.log` also says "toast activator registered", and the Start menu shortcut and the CLSID key above exist.
5. Uninstall from Settings → Apps (or the Start menu): the `rustling-tulip-daemon` value is gone from `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, the `HKCU\Software\Classes\CLSID\{354BE9DC-…}` key is gone, the Start menu entry is gone, the install directory is gone and no longer on the user PATH (the other PATH entries unchanged), and `%APPDATA%\leftos\rustling-tulip\config\` is still there.
