# Windows installer

`rustling-tulip.nsi` is a hand-written NSIS 3 script (Modern UI 2) that packages the three release binaries into a per-user setup exe. `.\rt.ps1 installer` builds the binaries and runs `makensis`; nothing is code-signed yet, so SmartScreen warns on first run ("Windows protected your PC" → More info → Run anyway).

## What it installs

A per-user install, no elevation: every path and registry key is under the current user, so nothing asks for an administrator prompt.

| Installed | Where |
|---|---|
| `rustling-tulip-native.exe` (the GPUI client), `rustling-tulipd.exe` (the daemon), `rt-tracer.exe` (the per-session PTY supervisor) | `%LOCALAPPDATA%\Programs\rustling-tulip\` |
| Uninstaller | `%LOCALAPPDATA%\Programs\rustling-tulip\uninstall.exe` |
| Start menu shortcut | `%APPDATA%\Microsoft\Windows\Start Menu\Programs\rustling-tulip.lnk` |
| Desktop shortcut (optional checkbox, off by default) | `%USERPROFILE%\Desktop\rustling-tulip.lnk` |
| Daemon autostart (optional checkbox, on by default) | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `rustling-tulip-daemon` = `"<install dir>\rustling-tulipd.exe" --detach` |
| Add/Remove Programs entry | `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\rustling-tulip` |

The installer adds **no Windows Firewall rule**: the owner runs Fort Firewall, and the daemon listens on loopback (and the tailnet listener, when enabled) without a firewall exception.

The finish page offers "Run rustling-tulip", which starts the client unelevated.

## Stopping the daemon

Before it copies files and before the uninstaller deletes them, the script stops a running daemon gracefully: it reads `%APPDATA%\leftos\rustling-tulip\config\daemon.json`, POSTs `http://127.0.0.1:<port>/shutdown` with the handshake's bearer token, then waits up to 5 s for the file to disappear. Every `rt-tracer.exe` supervisor survives, so a reinstall over a client with sessions open leaves those sessions alive — the next daemon reattaches to them. The daemon and tracer run from a content-addressed binary cache, so the installed exes are never locked by a live session.

## What uninstall removes and keeps

Removed: the three exes, `uninstall.exe`, the install directory, both shortcuts, the `rustling-tulip-daemon` autostart value and the uninstall registry key.

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

## App icon

`icon.ico` is the app icon for the installer, the uninstaller and the client exe: `apps/native/build.rs` embeds it as icon resource 1 (`apps/native/app.rc`), which GPUI loads for the window and taskbar and which Explorer, the shortcuts and the Add/Remove Programs entry show.

It is built from the SVGs in `icon/`: `icon.svg` for 24 px and up, and `icon-16.svg`, a glyph drawn for 16 px. After changing either, rebuild it from the repo root with `uv run --script tools/installer/icon/build_icon.py` and commit the `.ico`. The design and the concepts it was picked from are in `docs/plans/canvases/app-icon/`.

## Hand test

1. Run the setup exe with the default options. It installs under `%LOCALAPPDATA%\Programs\rustling-tulip`, adds the Start menu entry and the autostart value, and offers to run the client; it connects to a daemon.
2. Sign out and back in: the daemon starts at login (`%APPDATA%\leftos\rustling-tulip\config\logs\autostart.log` shows the launch) and the client connects without starting one itself.
3. With the client open and a session running, run the same setup exe again: it completes and the session survives (the tracer kept the PTY alive and the new daemon reattached).
4. Uninstall from Settings → Apps (or the Start menu): the `rustling-tulip-daemon` value is gone from `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, the Start menu entry is gone, the install directory is gone, and `%APPDATA%\leftos\rustling-tulip\config\` is still there.
