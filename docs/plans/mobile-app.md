# Mobile app (iOS and Android)

A phone client for the daemon, for the user's own use: check on sessions, answer them, and eventually work in them from anywhere. It is one more client of the same daemon, like the native client; the daemon, tracer and protocol stay the source of truth.

## Rulings

- **Scope, in phases:** monitor and respond first, then a full terminal, then the conversation view.
- **Reach: anywhere, through a relay.** This reverses the "attach beyond the LAN" and "mobile companion app" non-goals in [plan.md](../plan.md).
- **Relay:** a Rust binary in this workspace (`crates/relay`) on a small VPS (Fly.io or Hetzner). It pairs a daemon with a phone and forwards bytes it cannot read.
- **Encryption:** the pinned TLS of the LAN listener, run end to end through the relay. The phone does the same TLS handshake it would do on the LAN, pinned to the daemon's certificate fingerprint, inside the relay's WebSocket. The relay never holds a key, and one crypto path serves both the LAN and the relay.
- **Stack:** Flutter UI over a shared Rust core (protocol types, pinned-TLS connect, pairing profiles) through `flutter_rust_bridge`. No hand-kept protocol mirror.
- **Audience: the user only.** Android by sideload, iOS through TestFlight on a personal Apple Developer account. No store listing. This also keeps within Anthropic's personal-use terms (see [gui-mode-billing.md](../spikes/gui-mode-billing.md)).
- **Push notifications:** the daemon sends them itself (APNs and FCM) with the user's own keys. The relay plays no part in push.
- **iOS builds:** cloud macOS CI (GitHub Actions macOS runners or Codemagic) builds, signs and uploads to TestFlight. There is no Mac.
- **Priority:** after native client Phase 6, which brings back the pinned-TLS client and the pairing flow this app reuses.

## How it connects

```text
phone (Flutter UI)
  └─ Rust core: pinned-TLS client ──wss──▶ relay (VPS) ◀──wss── daemon, outbound, opt-in
                  └────────── TLS session, end to end ──────────┘   └─ same TLS acceptor as the LAN listener (lan.rs)
on the LAN: phone ──pinned TLS──▶ daemon LAN listener directly (mDNS discovery)
```

What already exists: the daemon's opt-in LAN TLS listener with a self-signed certificate (`crates/daemon/src/lan.rs`), short-code pairing (`pairing.rs`), mDNS discovery (`discovery.rs`) and constant-time secret checks (`secret.rs`). The client-side pinned-TLS tunnel and saved host profiles are in the Tauri app's `apps/tauri-app/src-tauri/src/remote.rs` on the `tauri-last` tag. It is a byte-level proxy that never parses WebSocket frames, which is why it can run inside a relay stream unchanged.

## Phases

Item ids are MA1, MA2, … (mobile app), distinct from the macOS plan's M0–M4.

- [ ] **MA1 Shared client core.** A crate (name to settle, for example `crates/remote-client`) holding the pinned-TLS connect, host profiles and pairing, recovered from `remote.rs`. Native client Phase 6 builds its remote mode on this crate rather than on its own copy, so the two share the code from the start.
- [ ] **MA2 Relay.**
  - `crates/relay`: a WebSocket rendezvous. A daemon registers under a random rendezvous id; a phone connects with that id plus its device credential; the relay pipes the two sockets' bytes together.
  - The daemon side is an opt-in setting: when on, it keeps an outbound connection to the relay, reconnects with backoff, and hands each relayed stream to the same TLS acceptor the LAN listener uses.
  - Deployment on the chosen VPS: a container, TLS on the relay's public endpoint (in addition to the end-to-end TLS inside), and restart on failure.
- [ ] **MA3 Per-device credentials.** Each paired phone gets its own token, revocable from the native client's Settings. Today one `lan.json` token is shared by every remote client, which is too coarse for a device carried outside the home.
- [ ] **MA4 Flutter shell and pairing.** The app skeleton, the Rust core through `flutter_rust_bridge`, and pairing by QR code shown in the native client's Settings. The QR code carries the certificate fingerprint, the relay endpoint, the rendezvous id and a one-time pairing code. LAN discovery through mDNS is used when the phone is at home.
- [ ] **MA5 iOS build pipeline.** CI on a macOS runner builds, signs and uploads to TestFlight. Needs a paid Apple Developer Program membership, a signing certificate and a provisioning profile, and an App Store Connect API key as CI secrets. Android builds locally on Windows.
- [ ] **MA6 Monitor and respond.** A session list grouped as in the sidebar, with live status and attention marks; the recent output of a session as plain text; reply to a prompt; answer a permission prompt; Stop and Resume. It's much better with hook-reported status ([borrowed-ideas.md](./borrowed-ideas.md)).
- [ ] **MA7 Push notifications.** A new additive protocol message registers a device's push token with the daemon. On `Attention`, the daemon sends APNs and FCM pushes with the user's keys, respecting the existing notification toggles. The keys live in the config dir, outside `state.json`.
- [ ] **MA8 Full terminal.** A terminal per session with a phone key bar (Esc, Tab, arrows, Ctrl), resizing that doesn't fight the desktop's size for the same session, and scrollback replay.
- [ ] **MA9 Conversation view.** Sessions as chats. Depends on the conversation-view item in [borrowed-ideas.md](./borrowed-ideas.md), and on `--print` staying on the subscription.

## Open questions

- **Relay authentication:** how the relay itself keeps strangers from using it, for example a relay secret the daemon and the phone both present, separate from the end-to-end TLS.
- **Recent output for MA6:** render the session's screen to text on the daemon, or send raw scrollback bytes and render them in the app.
- **Size:** settled (user, 2026-09-28): when the phone and the desktop show the same session (MA8), the PTY takes the size of the client that last sent input to it; Claude's terminal UI redraws on each switch.
- **Terminal renderer for MA8:** a Flutter terminal package such as `xterm` on pub.dev, or render in Rust (`alacritty_terminal`, as the native client does) and draw the grid in Flutter.
- **Cost and upkeep of the VPS:** provider, region, and how it gets updates.
