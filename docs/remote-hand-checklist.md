# Remote access hand checklist

This checklist verifies remote access over a real tailnet, which the automated tests cannot reach: the live e2e tier (`.\rt.ps1 native-e2e`) runs the real daemon against a fake `tailscale` on loopback, so it never meets a real `tailscale cert`, real `whois` answers, other Tailscale users or a firewall.

Run it by hand before a release that touches remote access (the daemon's tailnet listener, pairing, devices, the host picker, file fetch or autostart), and note each failure on the release's Linear issue. The design and its rulings are in [plans/remote-access.md](plans/remote-access.md).

## Before you start

- Two Windows PCs, signed in to the same tailnet as the same Tailscale user, both running Tailscale. Below, **the host** is the PC whose sessions are reached, and **the device** is the PC that reaches them.
- HTTPS certificates turned on for the tailnet in the Tailscale admin console (DNS page).
- For section 8: a node of another tailnet user, a tagged node, a node shared across tailnets, and a second Windows account on the host. A node that is not Windows can stand in where a step says `curl`.
- The same build of rustling-tulip on both PCs (the daemon serves only the current protocol version), from `.\rt.ps1 installer` or `.\rt.ps1 build -Release`.
- On the host, the logs to watch: `%APPDATA%\leftos\rustling-tulip\config\logs\daemon.log` and `native.log` beside it.
- `<host>` below is the host's full MagicDNS name without the trailing dot (`box.tail1234.ts.net`), and `<short>` its first label (`box`).

## 1. The Remote access switch on the host

- [ ] Open Settings → Remote access on the host with remote access never turned on. **Expect:** the section `Remote access (Tailscale)`, the switch `Allow remote access over Tailscale` off, and the line `Remote access is off.`
- [ ] Quit Tailscale on the host (system tray → Exit) and turn the switch on. **Expect:** `Tailscale is not running. Start it and remote access connects on its own.`
- [ ] Start Tailscale again without touching Settings. **Expect:** the line passes through `Starting…` and reaches `Serving on your tailnet.` by itself, with `Address: https://<host>:8787` and `Certificate expires in <n> days` under it.
- [ ] Sign out of Tailscale on the host (Tailscale menu → Log out). **Expect:** `Tailscale is signed out or stopped. Sign in to Tailscale to turn remote access on.` Sign in again. **Expect:** `Serving on your tailnet.` returns by itself.
- [ ] In PowerShell on the host, run `Get-NetTCPConnection -LocalPort 8787 -State Listen`. **Expect:** listeners on the host's `100.x.y.z` and `fd7a:115c:a1e0:…` addresses only, never `0.0.0.0` or `::`.
- [ ] Turn the switch off. **Expect:** `Remote access is off.`, the Address and certificate lines gone, nothing listening on 8787, and `daemon.log` showing `tailnet listener closed`. Turn it back on for the next sections.
- [ ] Optional, since it changes the whole tailnet: turn HTTPS certificates off in the admin console and turn the switch off and on. **Expect:** `HTTPS certificates are off for your tailnet. Enable HTTPS in the Tailscale admin console (DNS page).` and nothing listening on 8787. Turn HTTPS back on. **Expect:** serving resumes by itself.
- [ ] Optional: tag the host in the admin console. **Expect:** `This PC is tagged in Tailscale, so it has no owner to admit. Remote access stays down.` Remove the tag. **Expect:** serving resumes.
- [ ] Hold the port: with the switch off, run `python -m http.server 8787 --bind <host's 100.x address>`, then turn the switch on. **Expect:** `Port 8787 is in use on this PC's tailnet address. Choose another port.`
- [ ] Type `8788` in the `Port` field and press `Apply`. **Expect:** `Serving on your tailnet.` and `Address: https://<host>:8788`. Type `0` and press `Apply`. **Expect:** `Enter a port from 1 to 65535.` Set the port back to `8787` and stop the Python server.
- [ ] Close the host's window with `Keep sessions running in background` and wait a minute. **Expect:** the daemon keeps running (the hint `While remote access is on, the daemon keeps running when no window is open.`), and `Get-NetTCPConnection -LocalPort 8787 -State Listen` still lists the listeners.

## 2. The certificate

- [ ] In an unelevated PowerShell on the host, as the Windows user that runs Tailscale, run `tailscale cert --cert-file $env:TEMP\t.crt --key-file $env:TEMP\t.key <host>`. **Expect:** exit code 0 and both files written, with no UAC prompt. Delete the two files afterwards.
- [ ] With remote access serving, open `%LOCALAPPDATA%\leftos\rustling-tulip\data\tailnet\`. **Expect:** `<host>.crt` and `<host>.key`, and `daemon.log` showing `tailnet listener: certificate loaded`.
- [ ] On the device, open `https://<host>:8787/` in a browser. **Expect:** no certificate warning: the certificate is a public one for `<host>` (the page itself may be an error, since the listener serves only the client's WebSocket).
- [ ] When the host's certificate line reads `Certificate expires in 14 days` or fewer, leave the daemon running for a day. **Expect:** `daemon.log` shows `tailnet listener: certificate reloaded`, the line jumps back to about 90 days, and connected devices stay connected.

## 3. Pairing the device

- [ ] On the device, press `Connections` in the footer. **Expect:** the `Connections` dialog shows `Looking for your devices…`, then `This PC` (tagged `Current`) and `<host>` among the online peers.
- [ ] Type `box` (a bare label) under `Add a host` and press `Connect`. **Expect:** `Use the full name, for example box.tail1234.ts.net.` and nothing dialled.
- [ ] Pick the `<host>` row. **Expect:** the dialog closes, and the "Pair with `<host>`" dialog opens by itself with `<host> does not know this PC.` and the buttons `Ask to pair` and `Close`; the footer pill reads `<short> · not paired`.
- [ ] Press `Ask to pair`. **Expect:** the device shows `Approve this PC on <host>.` with an `expires in m:ss` countdown and `Cancel`; the host raises the Windows toast `A device asks for remote access` with `Approve…` and `Deny`, and Settings → Remote access on the host shows `Waiting devices · 1` with the device's name, its Tailscale login and an `m:ss left` countdown.
- [ ] On the host, press `Deny` on the toast. **Expect:** the device ends on `<host> turned this PC down.` with `Ask again` and `Close`, and the host's Waiting devices row is gone.
- [ ] On the device, press `Ask again`. **Expect:** a new toast and Waiting devices row on the host, as above.
- [ ] On the host, press `Approve…` on the toast. **Expect:** the window comes forward with the dialog `Approve <device>`, the line `Approving shows a one-time code for this device.` and `Approve`, `Deny` and `Close`; nothing is sent until `Approve` is pressed.
- [ ] Press `Approve`. **Expect:** the host shows a QR code, a six-digit code such as `735 160`, `or type on the phone` and `expires in m:ss · this request only`; the Waiting devices row reads `Approved · waiting for the device`; the device's dialog moves to `Type the code <host> shows.` with a `6 digits` field and `Pair`.
- [ ] Type a wrong code on the device and press `Pair`. **Expect:** `That code is not right.` under the field. Type the right code and press `Pair`. **Expect:** the dialog closes, the device connects, and its footer pill shows `<short> · <n> sessions`; the host's Waiting devices row is gone.
- [ ] On the host, open Settings → Remote access. **Expect:** the `Devices` section lists the device with `Connected` and a `Block` button.
- [ ] On the host, in a terminal, run `rustling-tulipd pair` (the daemon must be running). **Expect:** `Pairing code: <six digits>` and `Type it on the device's pairing screen within <n> minutes. It works once, for one device.`, and `daemon.log` shows `pairing: a CLI code was minted`. Typing that code belongs to the phone (see the last section): the native Pair dialog shows its code field only once the host has approved.
- [ ] Close the device's client and open it again. **Expect:** it boots straight onto `<host>` with no Pair dialog, since `native-ui.json` keeps `last_target` and the device token is in Credential Manager.

## 4. Driving the host from the device

- [ ] On the device, press `Shell` in the sidebar and run `hostname`. **Expect:** the host's computer name; the host's window shows the same session in its sidebar.
- [ ] Attach on the device to a Claude session started on the host and type a prompt. **Expect:** the output appears on both PCs, and the session's status moves to Working and back as on the host.
- [ ] On the device, open a session's menu. **Expect:** `Reveal worktree` and `Open in Explorer` are shown disabled, with the tooltip `Not available on a remote host: this opens files on this PC`.
- [ ] On the device, open the footer daemon flyout. **Expect:** `Reconnect now` in place of `Restart daemon`, and no Stop, Handshake file or Open daemon.log rows.
- [ ] On the device, open Settings. **Expect:** no `Remote access` tab: it configures the PC the client manages.
- [ ] Close the device's window. **Expect:** it quits at once with no Quit dialog, and the host's sessions keep running.

## 5. Block, forget and pair again

- [ ] With the device connected, press `Block` on its row in the host's `Devices` section. **Expect:** the dialog `Block <device>?` with `Its live connections end now, and it cannot pair again until you forget it.` and `Cancel` and `Block`.
- [ ] Press `Block`. **Expect:** the device's connection drops at once and, on its next connect, its footer pill reads `<short> · not paired`; the host's row shows `Blocked` with a `Forget` button.
- [ ] On the device, press the footer pill. **Expect:** "Pair with `<host>`" opens with `<host> no longer accepts this PC's pairing.`
- [ ] Press `Ask to pair`. **Expect:** `<host> did not accept this PC's request.` with `Ask again` and `Close`; the host raises no toast and shows no Waiting devices row.
- [ ] On the host, press `Forget` on the blocked row. **Expect:** the row is gone (`No device has paired yet.` when it was the only one).
- [ ] On the device, press `Ask again` and approve on the host as in section 3. **Expect:** the device pairs and connects, and the host's `Devices` row reads `Connected` again.
- [ ] Close the device's client and look at the host's row. **Expect:** `Last seen just now`, and later a longer `Last seen` time.

## 6. Turning remote access off while connected

- [ ] With the device connected, turn the host's switch off. **Expect:** the device's connection drops at once and its pill reads `<short> · disconnected` and then `<short> · connecting…` as it retries; nothing listens on 8787.
- [ ] Turn the host's switch back on. **Expect:** the device reconnects by itself, with no Pair dialog.
- [ ] Change the host's port to `8788` and `Apply` while the device is connected. **Expect:** the device drops and keeps retrying 8787. On the device, add `<host>` with Port `8788` under `Add a host`. **Expect:** it connects. Set the host's port back to 8787 afterwards.

## 7. A device removed from the tailnet

- [ ] With the device connected and a shell running on it, remove the device (or expire its key) in the Tailscale admin console and time how long the device's pill keeps showing `<short> · <n> sessions` and the host's row keeps showing `Connected`. **Expect:** both drop; record the seconds each took on the release issue. Add the device back to the tailnet afterwards.

## 8. Peers that are refused

- [ ] Open Tailscale's IP address doc (kb/1033) and compare the ranges it gives with `in_tailscale_range` in `crates/tailnet/src/rule.rs`. **Expect:** the doc states `100.64.0.0/10` and `fd7a:115c:a1e0::/48`, as the code does.
- [ ] From a node of another Tailscale user in the same tailnet, dial the host (the client's `Add a host` with `<host>`, or `curl -v https://<host>:8787/`). **Expect:** the connection is dropped before TLS: the client never reaches the Pair dialog, `curl` fails in the handshake rather than receiving an HTTP status, and `daemon.log` shows `tailnet listener: peer refused by the owner rule` with `reason=peer node belongs to another Tailscale user`.
- [ ] From a tagged node (`tailscale up --advertise-tags=tag:<tag>`), dial the host the same way. **Expect:** dropped the same way, with `reason=peer node is tagged`.
- [ ] Share the host to another Tailscale account (admin console → Share) and dial it from that account's node. **Expect:** dropped the same way; record which reason the log gives (`peer node belongs to another Tailscale user`, `peer node is shared in by another user` or `peer node is a sharee node`).
- [ ] With a node shared into the tailnet from another account, open `Connections` on the device. **Expect:** that node is not listed, and no tagged or offline node is either.
- [ ] Sign in to the host as a second Windows account, open the client there and add `<host>` under `Add a host`. **Expect:** dropped, with `reason=peer is the host node itself` in the first account's `daemon.log`.
- [ ] In that second account, open Settings → Remote access and turn the switch on. **Expect:** `Tailscale belongs to another Windows user or runs in server mode.` Turn it off and sign out of the account.
- [ ] After each refused dial, check the host's Settings. **Expect:** no toast and no Waiting devices row: a refused peer never reaches pairing.

## 9. Firewall and login launch

- [ ] In Fort Firewall on the host, allow the folder `%LOCALAPPDATA%\leftos\rustling-tulip\data\binaries\` rather than one exe. **Expect:** the device connects.
- [ ] Rebuild and restart the host's daemon (`.\rt.ps1 restart`, which runs a new content-addressed copy from that folder). **Expect:** the device reconnects with no new Fort Firewall prompt or rule, and no Windows Defender Firewall rule is needed.
- [ ] On the host, turn on `Start the daemon when I log in` (hint `So remote access works before you open this app.`). **Expect:** the switch stays on, and `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` holds `rustling-tulip-daemon` naming the daemon with `--detach`.
- [ ] Sign out of Windows on the host and sign in again, without opening the client. **Expect:** `logs\autostart.log` is rewritten, a `rustling-tulipd` process runs, and the device connects and lists the host's sessions.
- [ ] Turn the switch off. **Expect:** the `rustling-tulip-daemon` Run value is gone.

## 10. Fetched files from a remote repo

- [ ] On the device, connected to the host, Ctrl+click a path such as `src/main.rs:42` in a terminal of a session in a registered repo. **Expect:** the toast `Fetching main.rs from <host>`, then the file opens at line 42, saved under `%LOCALAPPDATA%\leftos\rustling-tulip\data\remote-files\<host>\<repo-name>\src\main.rs`.
- [ ] Open the sidebar's ⋯ menu → `Fetch file…`. **Expect:** the `Fetch file` dialog with the hint `Relative to <repo>. Add :line to open at a line.` and the placeholder `src/main.rs:42`; a fetch shows `Waiting for <host>…`, then its progress, and opens the file.
- [ ] Fetch an `.html` page in that repo whose images and stylesheet sit in relative folders. **Expect:** `Fetching <page> and its media (n of m)`, then `Fetched <page> and its media`, and the page opens with its images and styles, each file saved at its repo-relative place under the same folder.
- [ ] Fetch a page that names a missing image. **Expect:** `Some of <page>'s media didn't load`, and the page still opens.
- [ ] Start a large page fetch and turn the host's switch off mid-fetch. **Expect:** `Couldn't fetch <page>'s media` with `The connection was lost.`, the page does not open, and no `.fetch-*.part` file is left beside the targets.

## iPhone and PWA

The iPhone checks (installing the PWA from Safari, pairing with the QR code or the typed code, push, and re-approving a wiped phone or one whose storage Safari evicted) are PW.8 (RT-369) in [plans/mobile-app.md](plans/mobile-app.md).
