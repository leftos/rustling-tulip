# Mobile app (iPhone PWA over Tailscale)

A phone client for the daemon, for the owner's own use: check on sessions, answer them, and work in them from anywhere. It is a Progressive Web App installed on the owner's iPhone from the daemon's own tailnet listener, one more client of the same daemon like the native client; the daemon, tracer and protocol stay the source of truth. Platform facts with their sources are in [pwa-iphone.md](../research/pwa-iphone.md); the remote boundary is [remote-access.md](./remote-access.md).

## Rulings

- **A PWA on the iPhone.** The phone app is a web app the daemon serves, installed with Safari's Share, Add to Home Screen. The owner's primary phone is an iPhone; native apps can follow later if the owner decides to expand. Android Chrome runs the same pages, but nothing is built or checked for it.
- **Scope, in phases:** monitor and respond first, then a full terminal, then the conversation view.
- **Reach: Tailscale only.** The phone reaches the daemon's tailnet listener (`crates/daemon/src/tailnet_listener.rs`) with its `tailscale cert` TLS, a public-CA certificate Safari trusts with nothing pinned; there is no relay and no pinned TLS. This is the "attach from outside the tailnet" non-goal in [architecture.md](../architecture.md#non-goals) held, not reversed.
- **Per-device credentials:** each approved phone holds its own device token (TS.3b, RT-166), revocable from the native client's Settings by the block list (RT-96). A phone whose storage Safari evicted asks for a token again.
- **Audience: the owner only.** No store listing, no TestFlight, no Apple Developer account (Web Push to a Home Screen web app needs none, [pwa-iphone.md](../research/pwa-iphone.md) §1), and no iOS build pipeline. This also keeps within Anthropic's personal-use terms (see [gui-mode-billing.md](../spikes/gui-mode-billing.md)).
- **Push:** the daemon sends Web Push itself with the owner's own VAPID keys, through the push service the phone's subscription names (`web.push.apple.com` on iOS). Nothing else is in the path.
- **Badge:** the Home Screen icon's badge counts Needs you only, through the Badging API and the push payload.
- **Accepted losses:** no home-screen widget (RT-126) and no inline reply or action buttons on a notification (Safari supports no notification actions, [pwa-iphone.md](../research/pwa-iphone.md) §3); a tap opens the app at the session.
- **Recent output:** for monitor and respond, the daemon renders the session's screen to text (the visible screen and the last lines) and sends that, not raw scrollback.
- **Size:** when the phone and the desktop show the same session, the PTY takes the size of the client that last sent input to it (RT-184); Claude's terminal UI redraws on each switch.
- **What the app shows and how it alerts** is in [off-orca.md](./off-orca.md) (sections 2, 3 and 5).
- **Priority:** after Release 2's remote mode, whose device tokens (RT-166), block list (RT-96) and size rule (RT-184) the phone uses.

## Phone screens (PWA)

Owner rulings from the lane mocks under `docs/plans/canvases/` (RT-143); the phone borrows from herdr's collie (`X:\dev\_ext\AltanS\collie`).

- **Home** (`mobile-home`, lane C): collie's grouped list, whose rows never move when their state changes, with a swipeable Needs you tray on top holding one compact card at a time. Yes/no and permission prompts are answered from the tray; anything longer opens the session. The Home Screen badge counts Needs you only.
- **Session screen** (`mobile-session`, lane C): the conversation renders as chat down to the agent's status line, and everything from the status line down stays the live terminal as drawn. With no prompt pending, Quick and Keys sit behind composer buttons as in collie; a menu no hook reports falls back to the keys.
- **Chat look** (`mobile-chat`, lane A): collie's stream. Your turns are bubbles and Claude's prose is plain; each tool call is a one-line card (an Edit opens to its diff, a Bash shows its last output lines), and four or more calls in a row fold into one row naming the counts and the newest step.
- **Prompts** (`mobile-prompts`, lane C): a pending prompt rises as a bottom sheet over the chat, terminal and composer, with the answer buttons (each typing the option's number, never its label), Esc, up and down, and a Terminal toggle that shows Claude's dialog as drawn; a swipe puts it away. The sheet takes over the prompt state the session screen's dock had. An answered prompt becomes a one-line receipt in the chat.
- **Pairing** (`mobile-pairing`, lane B): the phone asks; a Windows toast on the host PC names the device and its Tailscale login, and Approve shows that request's QR and code, which the phone's scanner (opened by itself) reads. Settings → Remote access keeps a Waiting devices row where a missed toast is found again. A pairing code minted by the daemon from the command line, over SSH, approves a phone with no one at the PC.

## How it connects

```text
iPhone: Home Screen web app, Tailscale app connected
  ├─ GET https://{name}:{port}/…  ──tailscale cert TLS──▶ daemon tailnet listener: the PWA's files
  └─ wss://{name}:{port}/ws       ──same listener───────▶ Hello with the device token, then the usual protocol
daemon ──HTTPS POST, VAPID-signed, encrypted payload──▶ web.push.apple.com ──▶ iPhone (on or off the tailnet)
```

What already exists: the opt-in tailnet listener (switched by `remote.json` and `ConfigureRemote`) with `tailscale cert` TLS and certificate renewal, the owner rule in `crates/tailnet`, constant-time secret checks (`crates/daemon/src/secret.rs`), and the tailnet router (`build_tailnet_router` in `crates/daemon/src/server/remote.rs`), whose `/ws` upgrade already accepts `Origin` exactly `https://{name}:{port}`, the PWA's origin (invariant 6 in `remote-access.md`).

On a tailnet connection `Hello.auth_token` is the device's own token.

What the daemon gains:

- **The PWA's files on the tailnet router only.** `GET` routes beside `/ws` and `/health` serve the built app: the page, its hashed assets, `manifest.webmanifest`, the service worker and icons, with any other path that is not a file answered by the page so a deep link (`/s/<session id>`) opens the app. The loopback router serves none of it.

  Cache rule from collie: hashed assets `public, max-age=31536000, immutable`, everything else (the page, the service worker, the manifest) `no-cache`, so a stale service worker never pins an old build. Every response carries a `Content-Security-Policy` allowing only same-origin scripts and connections, and `X-Content-Type-Options: nosniff`, since the host's `*.ts.net` name is public in Certificate Transparency logs.
- **No state-changing HTTP route.** Every action, push registration included, travels over `/ws` after `Hello`, so the device-token check stays in one place (`server/handshake.rs`) and invariant 6's same-origin rule needs no second site.
- **Additive messages** (no protocol bump): push registration (PW.6) and the rendered-screen stream (PW.5). Home needs nothing new: `SessionSnapshot` already carries status and the hook-reported `pending_input`.
- **A stable origin** (invariant 11): the MagicDNS name and the configured port. Changing either makes a new origin, so the phone reinstalls the app, pairs again and subscribes again.

The page lives in `apps/pwa/`. The WebSocket dies whenever the app leaves the foreground (iOS suspends a backgrounded web app; [pwa-iphone.md](../research/pwa-iphone.md) §4), so the client reconnects and resyncs on `visibilitychange` and relies on push for alerts.

## Stack

**Orchestrator decision (open to the owner):** a TypeScript web UI with the protocol types generated from `crates/protocol`, not the Rust core in WASM; the full terminal drawn by xterm.js from the PTY stream already on the wire; and the session screen's live part drawn from rows the daemon renders from its own grid. The evidence:

- **The wire is JSON the browser already speaks.** Every message is a serde-tagged JSON enum (`#[serde(tag = "type", rename_all = "snake_case")]`) and `crates/protocol` depends only on serde, serde_json, chrono, uuid and thiserror (`crates/protocol/Cargo.toml`). WASM would add a `wasm32` target, `wasm-bindgen` glue and a JSON-to-Rust-to-JS round trip on every frame to obtain type definitions.

  Generated TypeScript types keep the "no hand-kept protocol mirror" rule: a test fails when the checked-in `.ts` file differs from what the generator emits. The page answers an unknown `type` or nested value with a default branch, as `InboundDaemonMessage::Unknown` and the `#[serde(other)] Unknown` variants do in Rust.
- **The full terminal needs no new daemon wire with xterm.js.** The PTY bytes (`data_b64`) and `LoadScrollback` replay are what the native client already feeds `alacritty_terminal`, so a page-side emulator takes the same stream, with cursor, input modes and selection, and the size rule (RT-184) gives it a phone-width PTY once the phone types.
- **The session screen's live part is cheaper from the daemon's grid.** The daemon keeps a per-session `alacritty_terminal` grid for status (`crates/daemon/src/screen.rs`: `rows`, `after_last_horizontal_rule`, `prompt_box_body`), and its screen rules already find the status line and the prompt box. Sending the rows from the status line down costs tens of lines over a phone link instead of a scrollback replay of up to 2 MB, and the slice the phone shows is the one the status rules read.

  Collie reaches the same conclusion for its whole mirror: it draws an already-rendered grid and refuses an emulator in the page ([ADR 0008](https://github.com/AltanS/collie/blob/HEAD/.adr/0008-collie-does-not-run-a-terminal-emulator.md), `X:\dev\_ext\AltanS\collie\.adr\0008-collie-does-not-run-a-terminal-emulator.md`). The full terminal is the case it does not cover, since collie has no terminal view to type into.

Toolchain: Vite builds `apps/pwa/` into `apps/pwa/dist`, with pnpm, oxlint and oxfmt and strict TypeScript per the `language-conventions` skill; unit tests in vitest; every build and test run through the gate (`tools/gate.ps1`). The service worker is hand-written, as collie's is (`web/src/sw.ts`).

## Push

- **VAPID keys:** a P-256 key pair the daemon generates the first time push is enabled, kept in a machine-local folder beside the tailnet certificate's (`ProjectDirs::data_local_dir().join("push")`, `%LOCALAPPDATA%\leftos\rustling-tulip\data\push\`, overridable with `RUSTLING_TULIP_PUSH_DIR` for tests), never in `state.json` and never in the roaming config dir. Replacing the keys invalidates every subscription, so nothing rotates them; deleting the folder is the reset.

  The JWT's subject is the PWA's origin URL (`https://{name}:{port}`), its audience the push service's origin and its `exp` under a day; one token per push service origin is cached and re-signed at most once an hour (Apple asks for no more; a bad JWT answers 403, [pwa-iphone.md](../research/pwa-iphone.md) §1).
- **One subscription per device.** The page subscribes from the tap on its Notifications switch, in the installed app (iOS requires the gesture and Home Screen install), with `userVisibleOnly: true`, and sends `{endpoint, p256dh, auth}` over `/ws`. The daemon stores it under the device the connection's token belongs to (`push/subscriptions.json`), so a new endpoint from the same device replaces the old one.

  Blocking the device (RT-96) deletes its subscription. Each app open compares the subscription it holds with the daemon's and re-registers when they differ; keyed by device, a re-subscribe never duplicates (the Apple forum report of `getSubscription()` returning null while pushes still arrive, [pwa-iphone.md](../research/pwa-iphone.md) §1).
- **What is sent:** a session entering Needs you (`AwaitingInput`), and a session finishing (a `Stop` with nothing running) when the device's Finished switch is on. The switches live with the device's subscription on the daemon, set from the PWA's Settings, since the desktop's toggles are client-side (`native-ui.json`). Wave 5's attention feed (AT.3) and presence routing (AT.6) later replace the trigger.
- **Declarative Web Push payload:** `{"web_push": 8030, "notification": {"title": …, "body": …, "navigate": "https://{name}:{port}/s/<session id>", "app_badge": "<Needs you count>"}}`. iOS 18.4 shows it with no service worker and never revokes for it; on iOS 16.4 to 18.3 the service worker's `push` handler shows the same JSON, and its `notificationclick` handler opens the `navigate` URL, closing the notification only after the open succeeds (collie's order, `web/src/sw.ts`).

  The payload holds the session's label and status words only, never terminal output: it shows on the Lock Screen and must stay under APNs' 4 KB.
- **Delivery options:** `TTL` of 6 hours, `Urgency: high`, and a `Topic` per session (`rt-` and 12 base64url characters of the session id's SHA-256, 15 characters) so an offline phone gets the latest alert per session. A test pins the topic rule Apple enforces: `[A-Za-z0-9_-]`, 32 characters at most, length not ≡ 1 (mod 4).
- **Errors:** 404 and 410 delete the subscription. 400 `BadDeviceToken` and other permanent failures count against a subscription only when the same push service accepted another subscription in that round, and five in a row delete it (collie's rule, `bridge/push.ts`); with one device that never fires, so it is logged instead.

  403 is the sender's fault (the VAPID JWT) and never prunes; 413 is a payload bug and is logged with its size; 429 and 5xx back off. Every failure logs the status and the push service's `reason`.
- **Sender:** RFC 8291 payload encryption (`aes128gcm`) and RFC 8292 VAPID signing on the `ring` backend the daemon's TLS already uses (`crates/daemon/Cargo.toml`), sent with `reqwest` (already in the workspace, `crates/daemon-client/Cargo.toml`); proved by RFC 8291's Appendix A test vector. The Windows host needs outbound HTTPS to `*.push.apple.com`; the phone needs no tailnet connection to receive.

## Badge

The badge is the number of sessions in Needs you. The page sets it with `navigator.setAppBadge(n)` (`0` clears it) on every snapshot while it runs, and every push carries the count in `app_badge` (or the service worker sets it on iOS before 18.4).

A push must show a notification, so no push lowers the badge silently: a prompt answered elsewhere while the app is closed leaves the count high until the app opens or the next push arrives. The badge shows once notifications are allowed ([pwa-iphone.md](../research/pwa-iphone.md) §2).

## Phases

Item ids are PW.1, PW.2, … (phone web app), distinct from the macOS plan's M0–M4. Each is one brief; daemon steps that touch `crates/protocol/src/lib.rs` or `crates/daemon/src/server.rs` run one at a time. Wave 4 holds PW.1 to PW.8; the later screens sit in their own waves.

- [ ] **PW.1 Serve the app shell.** `apps/pwa/` scaffold (Vite, TypeScript, a manifest with `display: standalone`, `start_url` and `scope` `./`, icons, a precaching service worker) and the daemon's static routes on `build_tailnet_router` with the cache and security headers above.

  Proved by router tests (a file, a hashed asset's immutable header, a deep link answered with the page, the loopback router serving none of it, the CSP header present) and a hand check: Add to Home Screen opens standalone.
- [ ] **PW.2 Protocol types and the connection.** The generator over `crates/protocol` with its staleness test; the page's socket (`wss://{name}:{port}/ws`, `Hello` with the stored device token, `Welcome.features`), reconnect with backoff and a resync on `visibilitychange`. Proved by the staleness test, vitest over the reconnect state machine, and a daemon test that a browser-shaped `Origin` handshake with a device token is served.
- [ ] **PW.3 Pairing on the phone.** After TS.3b (RT-166): the no-token screen sends a pending request, the camera scanner reads the host's QR code with the typed code beside it, and the token is kept in IndexedDB with `navigator.storage.persist()` asked for. Whether iOS offers `BarcodeDetector` is unverified, so the step checks it first and otherwise justifies a QR decoder dependency. Proved by vitest over the pairing flow against a scripted socket and the hand checklist.
- [ ] **PW.3b Host side of pairing** (RT-166, TS.3b). The Windows toast naming the device and its Tailscale login with Approve, the Waiting devices row in Settings → Remote access, and the pairing code the daemon mints from the command line. Proved by daemon tests for the CLI code (one-time, short-lived, whois-authorized device only) and UI specs for the toast action and the row.
- [ ] **PW.4 Home.** The grouped list with rows that never move, the Needs you tray with one compact card at a time, yes/no and permission answers typed as the option's number, Stop and Resume. Proved by vitest over the list model (a status change keeps row order) and a live e2e against an isolated daemon with fake-claude raising a prompt.
- [ ] **PW.5 Session screen, first cut.** The daemon's rendered rows from the status line down as a new additive subscription (debounced like the status watcher, unsubscribed on leave), the composer, Quick and Keys (collie's layouts), and the prompt sheet with its Terminal toggle. The chat above the status line arrives with PW.10. Proved by daemon tests over the row slice for the screen fixtures the status rules use, and vitest over the sheet's answer keys.
- [ ] **PW.6 Web Push.** The VAPID key store, the subscription messages, the sender with the payload, options and error handling above, the per-device Needs you and Finished switches, and the service worker's `push` and `notificationclick` handlers. Proved by the RFC 8291 vector, sender tests against a local fake push service (a 410 deletes, a 403 keeps, a replacement endpoint replaces, the topic rule), and the hand checklist.
- [ ] **PW.7 Badge.** `setAppBadge` from the Needs you count on each snapshot and on push for iOS before 18.4. Proved by vitest over the count and the hand checklist.
- [ ] **PW.8 Install guide and hand checklist.** A doc for turning remote access on, installing from Safari and pairing, and the checklist on the real iPhone: push while locked and off the tailnet, the tap landing on the session, the badge, a reinstall's new endpoint replacing the old one, and Safari's storage eviction asking for a token again.
- [ ] **PW.9 Full terminal** (Wave 8, RT-108). xterm.js fed `Output` and `LoadScrollback`, collie's key bar (Esc, Tab, Shift+Tab, arrows, Ctrl), the PTY size through RT-184.
- [ ] **PW.10 Chat** (Wave 8, RT-109). The chat mirror's entries (RT-51) above the status line, in the chat look ruled above.
- [ ] **PW.11 Linear cards** (Wave 8, RT-142). A tapped identifier shows the desktop's card from the daemon's `LinearIssues` cache.
- [ ] **PW.12 Files** (RT-106). A tapped path opens a viewer with Share (`navigator.share`) and Save; a worktree file tree; sending a photo or file into a session from the page's file picker and camera, since iOS has no Web Share Target.
- [ ] **PW.13 Accounts screen** (Wave 12, RT-112). Every Claude account with its 5h and 7d usage and reset times, Switch, and the auto-switch on/off and threshold, over the messages in [accounts.md](./accounts.md); adding, removing and logging in stay on the desktop.
- [ ] **PW.14 Dictation** (Wave 14, RT-83). The keyboard's own dictation key into the composer (decision below); design in [dictation.md](./dictation.md).

## Prior art for the phases

From herdr's collie (`X:\dev\_ext\AltanS\collie`, MIT, read at 82c6bba7), which serves its PWA over Tailscale:

- **PW.1, serving.** Collie's bridge serves the Vite build from `web/dist` (`bridge/server.ts`, `WEB_DIR` and `serveStatic`) with `cacheControlFor`: hashed `assets/` immutable, everything else `no-cache`, so a stale `sw.js` never wedges updates. Its manifest is built by `vite-plugin-pwa` with `injectManifest` (`web/vite.config.ts`, the `VitePWA` block: relative `start_url` and `scope`, no `id`, `display: standalone`).

  Collie puts `tailscale serve --bg 8787` in front for HTTPS and trusts `Tailscale-User-Login` (`README.md`, `docs/install.md`); rustling-tulip terminates TLS itself and asks whois about the real peer, so no header is trusted.
- **PW.1, PW.6, service worker.** `web/src/sw.ts`: Workbox precache with an SPA navigation fallback, `skipWaiting` and `clientsClaim`, a `push` handler that never drops a push silently, and a `notificationclick` that closes the notification only after the open succeeds.
- **PW.3, pairing.** `bridge/pairing.ts`: a one-time code stored as its SHA-256 with a 10-minute life and five attempts, a 256-bit device token returned once and stored as its SHA-256, and both files re-read per request so a CLI-minted code needs no restart.
- **PW.6, push.** `bridge/push.ts`: one JSON file of subscriptions with metadata, prune on 404/410, same-origin-witnessed eviction after five failures, the device naming the endpoint it replaces (`replaces`, kept in `web/src/lib/push-endpoint.ts`), and the base64-length topic rule. `cli/push-keys.ts` makes VAPID keys with `node:crypto` alone (a P-256 pair, base64url). Its `/api/subscribe` POST (`bridge/server.ts`) is the HTTP route this plan replaces with a `/ws` message.
- **PW.9, terminal.** Collie draws Herdr's rendered rows and refuses xterm.js ([ADR 0008](https://github.com/AltanS/collie/blob/HEAD/.adr/0008-collie-does-not-run-a-terminal-emulator.md)); PW.5 follows it, PW.9 does not, for the reasons under Stack.

From Orca (`github.com/stablyai/orca`) and ThinkTerm (`github.com/RoversX/ThinkTerm`, GPLv3: ideas only):

- **Revocation (RT-96).** Orca gives each paired client its own token, disconnects its live connections the moment it is revoked, and replaces an unused access link when a new one is made (`docs/site/content/docs/remote-servers.mdx`). ThinkTerm stores its browser tokens as digests with a TTL, with `mint`, `list` and `revoke` (`docs/thinkterm/web-access.md`).
- **PW.2, version check both ways.** Orca's handshake answers `client-too-old` or `server-too-old` with the version required, so the phone can say "update the desktop app" instead of failing (`src/shared/protocol-compat.ts`). A cached PWA older than its daemon is the same case.
- **PW.4, PW.13, retries that apply once.** Orca's phone journals each spend of a Codex rate-limit reset credit so a retry over a flaky link cannot spend two (`docs/site/content/docs/mobile.mdx`, accounts screen). Any phone action that isn't naturally idempotent, such as an account switch or create-session, needs the same.
- **PW.6, push only when away.** Orca pushes to the phone only while the desktop is locked or idle for 180 s or more, and pushes anyway when it cannot read presence (`src/main/notifications/desktop-away-state.ts`). On Windows that needs the session's idle time and lock state.
- **PW.9, size.** ThinkTerm's browser client claims a tab at its own size, and the desktop takes it back only on interaction there (`docs/thinkterm/web-access.md`), which matches the size ruling above.

## Open questions

None open. Orchestrator decisions, open to the owner:

- **The stack** is the one under Stack.
- **The built app ships as a `pwa/` folder beside the daemon exe**, which the binaries cache (`crates/daemon/src/paths.rs`) copies as a tree, built by `rt.ps1 build` and the installer; the daemon is never embedded with it, so a daemon build needs no node or pnpm (the gate's builders included).
- **Every Needs you pushes** while the device's Needs you switch is on, until presence routing (AT.6) decides what to hold; the switch is the way to mute it at the desk.
- **Dictation on the phone is the keyboard's own dictation key** into the composer: the Web Speech API is reported broken in Home Screen web apps ([pwa-iphone.md](../research/pwa-iphone.md) §4), and recording audio for the daemon to transcribe would break [dictation.md](./dictation.md)'s "only text crosses the wire". PW.14 checks that the composer takes it.
