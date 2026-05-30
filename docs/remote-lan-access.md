# Remote LAN Access — User Guide

Control your desktop's shells from a laptop on the same network. The shells keep
running on the desktop's hardware against the desktop's repos — the laptop is
just another window onto them, like a remote editor session (not a screen
share). You can pick up shells that are already running, spawn new ones, and
kill them, all over an encrypted connection.

There are two machines in this guide:

- **Host** — the machine whose shells you want to reach (e.g. your desktop).
- **Client** — the machine you're sitting at (e.g. your laptop).

Both run the same rustling-tulip app.

---

## 1. Turn on LAN access (host)

1. Open **Settings → Remote access (LAN)**.
2. Click **Enable LAN access**. (The default port is **8787**; change it first
   if that port is taken.)

That's it. LAN access is **off by default** — until you enable it, the daemon
only listens on `127.0.0.1` and nothing is reachable from the network. While
it's on, the host also announces itself on the LAN (mDNS) so clients can find it
without you typing an address.

> ⚠ **A paired client gets full control of this machine's shells** — it can run
> commands here. Only pair devices you trust, on a network you trust.

### Keep the host reachable after a reboot (Windows)

In the same panel, click **Start daemon on login**. After you sign in, the
daemon starts on its own and re-enables LAN access from your saved settings — no
need to open the app on the host. Click **Don't start daemon on login** to undo.

---

## 2. Connect from the client

Open the connection picker from the **Connections** button in the footer. You
have two ways to pair.

### Option A — Discover + pairing code (recommended)

1. On the host: **Settings → Remote access → Pair a device**. A **6-digit code**
   appears with a countdown (valid for 3 minutes).
2. On the client: in the connection picker, under **Discover on LAN**, click
   **Scan**. Your host appears in the list.
3. Click the host, type the code, and click **Pair**.

The client verifies the host's certificate, exchanges the code for an access
token, saves the host as a profile, and connects. You never copy the long token
by hand.

The code is single-use and short-lived: it expires after 3 minutes, and after
5 wrong attempts the window closes and the host must generate a new one.

### Option B — Connection code (fallback)

If discovery doesn't work on your network (some Wi-Fi/guest networks block
mDNS), use the code instead:

1. On the host: **Settings → Remote access**. Copy the **Connection code**
   (pick the right address first if the host has more than one).
2. On the client: connection picker → **Add remote (paste code)** → paste →
   **Connect**.

The connection code already contains everything needed to connect, so treat it
like a password.

Saved hosts show up in the picker for one-click reconnect. Boot reconnects to
whichever host (or Local) you used last.

---

## 3. Your own layout

Sessions are **shared** — every connected machine sees the same list of running
shells in the sidebar. But each machine keeps its **own tabs and pane layout**,
so curating your laptop's view doesn't disturb the desktop's.

The first time a new machine connects, it asks how to start:

- **Start empty** — build your own layout from scratch.
- **Open all sessions** — one pane per running shell.
- **Adopt the previous layout** — take over the pre-existing desktop layout
  (offered once, right after upgrading).
- **Clone another machine's layout** — copy a layout you've already set up
  elsewhere (same shells, fresh panes).

After that, reconnecting restores *your* machine's layout. If anyone kills a
session, the panes showing it close automatically on every machine.

> Two machines viewing the same shell share its terminal size (last one to
> resize wins). In practice each machine usually views a different set of
> shells, so this rarely comes up.

---

## 4. What works remotely, and what doesn't

Everything about the shells works: view live output, type input, spawn sessions,
kill them, browse git status.

Actions that would touch **your laptop's** filesystem instead of the host's are
hidden or disabled while connected to a remote host, because they'd point at the
wrong machine:

- Add a repo / create a workspace via the file picker
- Reveal in Explorer
- Open in VS Code

Where it makes sense, path fields stay typeable so you can still enter a
**host** path by hand.

---

## 5. Security at a glance

- **Encrypted.** All traffic uses TLS. The connection is pinned to the host's
  exact certificate — a different certificate is refused, so you'll know if
  something on the network is impersonating your host.
- **Opt-in.** LAN access is off until you enable it; loopback-only is the
  default.
- **Token = control.** The connection code and the access token both grant full
  control of the host's shells. Don't share them. Disabling LAN access on the
  host cuts off remote clients immediately.
- **Pairing trust.** The pairing flow is built for a trusted LAN. The host's
  certificate fingerprint travels over mDNS, so a determined attacker on the
  *same* network could in theory impersonate the host during pairing. The
  pasted connection code (Option B) avoids that, because its fingerprint is
  carried out-of-band.

---

## 6. Troubleshooting

| Symptom | Fix |
|---|---|
| **Scan finds nothing** | First time, Windows Firewall prompts to allow the app on the network (mDNS uses UDP 5353) — allow it on both machines. Some Wi-Fi / guest networks block mDNS between devices; use the connection code (Option B) instead. |
| **"Certificate fingerprint mismatch"** | The host's certificate changed (e.g. it was reset) or something is impersonating the host. Re-pair from a freshly copied connection code, or re-scan. |
| **"Too many attempts"** | The 6-digit code was entered wrong 5 times and the window closed. On the host, click **Pair a device** again for a new code. |
| **Code expired** | Codes last 3 minutes. Generate a new one on the host. |
| **Can't reach the host after reboot** | Make sure **Start daemon on login** is enabled on the host (Windows), and that you've signed in on the host (the daemon runs in your user session). |
| **A button is greyed out / "unavailable on remote connections"** | That action targets the local filesystem — see section 4. Switch back to **Local** in the connection picker to use it. |

---

## Quick reference

| | |
|---|---|
| Default port | `8787` |
| Discovery | mDNS (`_rustling-tulip._tcp`), UDP 5353 |
| Pairing code | 6 digits, 3-minute expiry, 5 attempts |
| Transport | TLS with certificate pinning |
| Host settings | Settings → Remote access (LAN) |
| Client picker | footer → Connections |
