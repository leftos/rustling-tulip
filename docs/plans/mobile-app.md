# Mobile app (iOS and Android)

> **Superseded in part:** RT-143 re-plans the phone as an iPhone PWA over Tailscale HTTPS (`remote-access.md`); the Android-first order, Flutter and `flutter_rust_bridge`, the pinned TLS and the relay, and APNs/FCM are superseded, and the MA steps are re-cut with that issue. This repo has no `crates/relay`, and `apps/` holds only the native client.

A phone client for the daemon, for the user's own use: check on sessions, answer them, and eventually work in them from anywhere. It is one more client of the same daemon, like the native client; the daemon, tracer and protocol stay the source of truth.

## Rulings

- **Scope, in phases:** monitor and respond first, then a full terminal, then the conversation view.
- **Reach: anywhere, over Tailscale or through the relay, both built from the start** . This reverses the "attach beyond the LAN" and "mobile companion app" non-goals in [architecture.md](../architecture.md#non-goals). The relay (MA2, RT-94) and per-device credentials (MA3, RT-96) are built in Wave 3 with the remote desktop, which uses them too. (Superseded: no relay and no pinned TLS; Tailscale is the only remote path, RT-143 and `remote-access.md`.)
- **Android first.** The MVP is Android (RT-38 and its sub-issues), built and sideloaded from Windows; the iOS pipeline (MA5, RT-117) is in the backlog. What the app shows and how it alerts is in [off-orca.md](./off-orca.md) (sections 2, 3 and 5). (Superseded: the phone is an iPhone PWA over Tailscale, RT-143.)
- **Relay:** a Rust binary in this workspace (`crates/relay`) on a small VPS (Fly.io or Hetzner). It pairs a daemon with a phone and forwards bytes it cannot read. (Superseded: this repo has no `crates/relay`; RT-143.)
- **Encryption:** the tailnet listener serves `tailscale cert` TLS and nothing is pinned (`remote-access.md`).
- **Stack:** Flutter UI over a shared Rust core (protocol types, pinned-TLS connect, pairing profiles) through `flutter_rust_bridge`. No hand-kept protocol mirror. (Superseded: no Flutter; the phone is a PWA, RT-143.)
- **Audience: the user only.** Android by sideload; iOS, when it comes, through TestFlight on a personal Apple Developer account. No store listing. This also keeps within Anthropic's personal-use terms (see [gui-mode-billing.md](../spikes/gui-mode-billing.md)).
- **Push notifications:** the daemon sends them itself (APNs and FCM) with the user's own keys. The relay plays no part in push. (Superseded: APNs/FCM are re-planned with the PWA, RT-143.)
- **iOS builds:** cloud macOS CI (GitHub Actions macOS runners or Codemagic) builds, signs and uploads to TestFlight. There is no Mac.
- **Priority:** after native client Phase 6, which builds the remote client this app reuses.

## Phone screens (PWA)

Owner rulings from the lane mocks under `docs/plans/canvases/` (RT-143); the phone borrows from herdr's collie (`X:\dev\_ext\AltanS\collie`).

- **Home** (`mobile-home`, lane C): collie's grouped list, whose rows never move when their state changes, with a swipeable Needs you tray on top holding one compact card at a time. Yes/no and permission prompts are answered from the tray; anything longer opens the session. The Home Screen badge counts Needs you only.
- **Session screen** (`mobile-session`, lane C): the conversation renders as chat down to the agent's status line, and everything from the status line down stays the live terminal as drawn. With no prompt pending, Quick and Keys sit behind composer buttons as in collie; a menu no hook reports falls back to the keys.
- **Chat look** (`mobile-chat`, lane A): collie's stream. Your turns are bubbles and Claude's prose is plain; each tool call is a one-line card (an Edit opens to its diff, a Bash shows its last output lines), and four or more calls in a row fold into one row naming the counts and the newest step.
- **Prompts** (`mobile-prompts`, lane C): a pending prompt rises as a bottom sheet over the chat, terminal and composer, with the answer buttons (each typing the option's number, never its label), Esc, up and down, and a Terminal toggle that shows Claude's dialog as drawn; a swipe puts it away. The sheet takes over the prompt state the session screen's dock had. An answered prompt becomes a one-line receipt in the chat.
- **Pairing** (`mobile-pairing`, lane B): the phone asks; a Windows toast on the host PC names the device and its Tailscale login, and Approve shows that request's QR and code, which the phone's scanner (opened by itself) reads. Settings → Remote access keeps a Waiting devices row where a missed toast is found again. A pairing code minted by the daemon from the command line, over SSH, approves a phone with no one at the PC.

## How it connects

```text
phone (Flutter UI)
  └─ Rust core: pinned-TLS client ──wss──▶ relay (VPS) ◀──wss── daemon, outbound, opt-in
                  └────────── TLS session, end to end ──────────┘   └─ same TLS acceptor as the tailnet listener
on the tailnet: phone ──tailscale cert TLS──▶ daemon tailnet listener directly
```

What already exists: the daemon's opt-in tailnet listener (`crates/daemon/src/tailnet_listener.rs`, switched by `remote.json` and `ConfigureRemote`) with `tailscale cert` TLS, the owner rule in `crates/tailnet` and constant-time secret checks (`secret.rs`). Per-device approval is TS.3b (`remote-access.md`). The Tauri app's pinned-TLS tunnel and saved host profiles (`apps/tauri-app/src-tauri/src/remote.rs` on the `tauri-last` tag) are history only.

## Phases

Item ids are MA1, MA2, … (mobile app), distinct from the macOS plan's M0–M4.

- [ ] **MA1 Shared client core.** A crate (name to settle, for example `crates/remote-client`) holding the tailnet connect and host profiles. Native client Phase 6 builds its remote mode on this crate rather than on its own copy, so the two share the code from the start.
- [ ] **MA2 Relay.**
  - `crates/relay`: a WebSocket rendezvous. A daemon registers under a random rendezvous id; a phone connects with that id plus its device credential; the relay pipes the two sockets' bytes together.
  - The daemon side is an opt-in setting: when on, it keeps an outbound connection to the relay, reconnects with backoff, and hands each relayed stream to the same TLS acceptor the tailnet listener uses.
  - Deployment on the chosen VPS: a container, TLS on the relay's public endpoint (in addition to the end-to-end TLS inside), and restart on failure.
- [ ] **MA3 Per-device credentials.** Each approved phone gets its own token, revocable from the native client's Settings (TS.3b in `remote-access.md`).
- [ ] **MA4 Flutter shell and pairing.** The app skeleton, the Rust core through `flutter_rust_bridge`, and pairing by QR code shown in the native client's Settings. The QR code carries the certificate fingerprint, the relay endpoint, the rendezvous id and a one-time pairing code.- [ ] **MA5 iOS build pipeline.** CI on a macOS runner builds, signs and uploads to TestFlight. Needs a paid Apple Developer Program membership, a signing certificate and a provisioning profile, and an App Store Connect API key as CI secrets. Android builds locally on Windows.
- [ ] **MA6 Monitor and respond.** A session list grouped as in the sidebar, with live status and attention marks; the recent output of a session as plain text; reply to a prompt; answer a permission prompt; Stop and Resume. It's much better with hook-reported status ([borrowed-ideas.md](./borrowed-ideas.md)).
- [ ] **MA7 Push notifications.** A new additive protocol message registers a device's push token with the daemon. On `Attention`, the daemon sends APNs and FCM pushes with the user's keys, respecting the existing notification toggles. The keys live in the config dir, outside `state.json`.
- [ ] **MA8 Full terminal.** A terminal per session with a phone key bar (Esc, Tab, arrows, Ctrl), resizing that doesn't fight the desktop's size for the same session, and scrollback replay.
- [ ] **MA9 Conversation view.** Sessions as chats. Depends on the conversation-view item in [borrowed-ideas.md](./borrowed-ideas.md), and on `--print` staying on the subscription.
- [ ] **MA10 Accounts screen.** Every Claude account with its 5h and 7d usage and reset times, a Switch button, and the auto-switch on/off and threshold, over the messages in [accounts.md](./accounts.md). Adding, removing and logging in stay on the desktop. MA7 sends auto-switch and all-exhausted events as pushes under their own toggle.
- [ ] **MA11 Dictation.** A tap-or-hold mic button on the MA6 reply bar and the MA8 key bar opens a review sheet bound to the session, filled by the phone's own recognizer (`SFSpeechRecognizer`, Android `SpeechRecognizer`) with partial results; Insert, Insert and send, Discard. Only text reaches the daemon. Design in [dictation.md](./dictation.md).

## Prior art for the phases

Details from Orca (`github.com/stablyai/orca`, which ships a phone app and a remote runtime) and ThinkTerm (`github.com/RoversX/ThinkTerm`, GPLv3: ideas only, which has browser and phone clients). None is adopted yet; each names the phase it would land in.

- **MA1, client core layering.** ThinkTerm's session crate takes time, spawning, the wire and events through host traits, so it builds for `wasm32` and its phone bindings share it; pure decision rules (resize, watchdog) sit in their own module, unit-tested off the network (`thinkterm-session/src/lib.rs`, `decide.rs`). The shared client core would keep its session logic free of GPUI and tokio the same way.
- **MA1, MA2, bounded receive queue.** ThinkTerm caps its receive queue by bytes and by chunk count and fails loudly when full (`thinkterm-session/src/byte_queue.rs`), so a slow phone link cannot grow memory without limit.
- **MA3, revocation.** Orca gives each paired client its own token, disconnects that client's live connections the moment it is revoked, and replaces an unused access link when a new one is made (`docs/site/content/docs/remote-servers.mdx`). ThinkTerm stores its browser tokens as digests and gives them a TTL, with `mint`, `list` and `revoke` (`docs/thinkterm/web-access.md`).
- **MA4, version check both ways.** Orca's handshake answers `client-too-old` or `server-too-old` with the version required, so the phone can say "update the desktop app" instead of failing (`src/shared/protocol-compat.ts`).
- **MA6, MA10, retries that apply once.** Orca's phone journals each spend of a Codex rate-limit reset credit so a retry over a flaky link cannot spend two (`docs/site/content/docs/mobile.mdx`, accounts screen). Any phone action that isn't naturally idempotent, such as an account switch or create-session, needs the same.
- **MA7, push only when away.** Orca pushes to the phone only while the desktop is locked or idle for 180 s or more, and pushes anyway when it cannot read presence (`src/main/notifications/desktop-away-state.ts`). On Windows that needs the session's idle time and lock state.
- **MA8, size.** ThinkTerm's browser client claims a tab at its own size, and the desktop takes it back only on interaction there (`docs/thinkterm/web-access.md`), which matches the settled "last client to send input" rule below.

## Open questions

- **Relay authentication:** settled (user, 2026-09-28): a shared relay secret, set up at pairing, that the daemon and each paired phone present; the relay drops anything else. It is separate from the end-to-end TLS.
- **Recent output for MA6:** settled (user, 2026-09-28): the daemon renders the session's screen to text (the visible screen and the last lines) and sends that, not raw scrollback.
- **Size:** settled (user, 2026-09-28): when the phone and the desktop show the same session (MA8), the PTY takes the size of the client that last sent input to it; Claude's terminal UI redraws on each switch.
- **Terminal renderer for MA8:** settled (user, 2026-09-28): `alacritty_terminal` in the shared Rust core, as the native client does, with the cell grid drawn in Flutter.
- **Cost and upkeep of the VPS:** provider, region, and how it gets updates. Deferred to MA2 (user, 2026-09-28).
