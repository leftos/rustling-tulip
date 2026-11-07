# Driving the iPhone Simulator on the Mac mini

`tools/sim/sim.ps1` drives Xcode's iPhone Simulator on the Mac mini builder (`builder@mac-mini`, reached by key-based ssh from this PC), so a session on this PC can put the phone app in a simulator and try to break it. RT-1088's driven run is the first caller.

Booting, opening a URL and screenshots go through `xcrun simctl`. Tapping, typing, swiping, pressing the Home button and reading the screen's accessibility tree go through [idb](https://github.com/facebook/idb), Meta's iOS Development Bridge, because `simctl` can do none of those.

Every verb is one or more ssh calls, each run through a login `zsh` so the Mac's login PATH applies. The script declares no parameters on purpose, as `tools/rt-mac.ps1` explains, so every argument reaches its verb as typed.

## The verbs

```powershell
.\tools\sim\sim.ps1 setup                    # once per Mac: idb 1.7.8 into ~/.local, no admin rights
.\tools\sim\sim.ps1 boot                     # boot iPhone 17 (the default) and wait until it is up
.\tools\sim\sim.ps1 boot "iPhone 18 Pro"     # the same for another installed device
.\tools\sim\sim.ps1 open https://example.com # open a URL in Mobile Safari
.\tools\sim\sim.ps1 shot .tmp\phone.png      # screenshot to a PNG on this PC
.\tools\sim\sim.ps1 tap 201 816              # tap a point
.\tools\sim\sim.ps1 swipe 100 600 100 100    # swipe between two points, 0.4 s unless a duration is given
.\tools\sim\sim.ps1 swipe 100 600 100 100 1  # the same over one second: a fast swipe does not scroll a web view
.\tools\sim\sim.ps1 home                     # press the Home button
.\tools\sim\sim.ps1 text "rustling"          # type into the focused field
.\tools\sim\sim.ps1 key 40                   # press a key by idb keycode
.\tools\sim\sim.ps1 terminate com.apple.webapp # terminate an app on the booted simulator
.\tools\sim\sim.ps1 describe                 # the screen's accessibility tree, as JSON
.\tools\sim\sim.ps1 shutdown                 # shut the booted simulator down
.\tools\sim\sim.ps1 reset                    # kill idb and its companion after a stale connection
```

`describe` is how a run finds something to tap: every element carries a `frame` in points (`x`, `y`, `width`, `height`), and `tap` and `swipe` take points, never pixels. A screenshot is 3× the points on the current iPhone devices, so a frame's centre is `x + width/2`, `y + height/2`, unmultiplied.

Every verb after `setup` needs a booted simulator and says `sim: no booted simulator; run sim.ps1 boot` when there is none.

## No Simulator window opens

`boot` boots the device and waits for it; it opens no window on the Mac's desktop, because Xcode 27 no longer ships Simulator.app (`Contents/Applications` holds `DeviceHub.app` instead) and nothing needs one: `simctl io` and `idb ui` both work against the headless device. The headless device has no hardware keyboard attached, so typing raises the on-screen keyboard, which is what a run that mirrors the phone wants to see.

## Print-only mode

`SIM_PRINT_ONLY=1` makes every verb print the ssh and scp command lines it would run and exit 0, touching nothing. A device id the script would look up on the Mac prints as `<udid>`, and the remote command appears inside the ssh word with its quoting intact. The Pester tests use only this mode:

```powershell
pwsh -NoProfile -Command "Invoke-Pester -Path tools/sim/sim.tests.ps1 -Output Detailed"
```

## Failures

A remote command that fails ends the script with exit 1 and `sim: <verb> failed: <the last line it wrote to stderr>`. A verb that needs arguments it did not get, or an unknown verb, prints the verb list and exits 2. The idb install in `setup` checks both downloads against their SHA-256 first, so a bad download installs nothing.

When a device verb fails with `dtuhidd connection ... invalidated`, the idb companion on the Mac has gone stale: `sim.ps1 reset` kills idb and its companion in one ssh call, and the next device verb starts a fresh one.

## What the Mac needs

- A person logged in at the console: the simulator runs in that GUI session (see `docs/plans/macos-compat.md`, "Native client", for what does and does not work from an ssh login).
- Xcode with an iOS simulator runtime installed (the device list `boot` reads comes from `xcrun simctl list devices`).
- `uv` at `/Users/builder/gate-tools/bin/uv`, which `setup` uses to install the idb client as a tool.

`setup` installs idb 1.7.8 into `builder`'s home with no admin rights: the companion from the v1.7.8 release's macOS arm64 tarball, checked against its SHA-256, unpacked whole into `~/.local/opt/idb-companion-1.7.8/` (its `idb_companion` needs the `Resources/` and resource bundles beside it) with `~/.local/bin/idb_companion` symlinked to it, and the v1.7.8 client wheel from the same release installed as a `uv` tool. Each half is installed only when it is missing, so re-running `setup` is cheap. `~/.local/bin` is not on the Mac's login PATH, so the device verbs call the client by path and point `IDB_COMPANION_PATH` at the companion.

Those two installs are the only ones this script makes on the shared builder; it needs no `sudo`, no Homebrew writes, and writes nothing outside `~/.local` and `/tmp`.

## Screenshots come back over scp

`shot` writes `/tmp/sim-rt-shot.png` on the Mac with `xcrun simctl io booted screenshot`, copies it to the local path you name with `scp`, removes the remote file and prints the local path. Two runs at once would share that one remote file name.
