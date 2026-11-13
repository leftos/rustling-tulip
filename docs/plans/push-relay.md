# Push relay (RT-1137)

A small hosted service, run by the owner, that holds the Apple and Firebase push keys and forwards a daemon's alerts to the native phone app on its users' phones. It is outbound only: a daemon posts to it, nothing comes in to the PC, and the daemon still accepts network connections only from the tailnet ([architecture.md](../architecture.md#non-goals)). The phone app it serves is RT-1266's GPUI app; the PWA keeps its direct Web Push ([mobile-app.md](./mobile-app.md), "Push").

## The problem

A store or TestFlight app has one bundle ID, and APNs accepts pushes for it only when they are signed with a key of the Apple team that owns it; FCM likewise accepts them only from a service account of the Firebase project the app is built against ([native-mobile-path.md](../research/2026-11-07-native-mobile-path.md), "Platform facts").

The product is released to more than its owner, and every user's daemon is self-hosted, so neither key can ship in the installer: anyone holding it could push to every user of the app.

Web Push has no such problem, since each daemon signs with its own VAPID key and the push service needs no account. The relay keeps that shape for native apps: each daemon still identifies itself with its own key, and only the relay holds the platform keys.

## How a push travels

```text
phone app ──RegisterPush { apns { token, environment, p256dh, auth } }──▶ its own daemon (over the tailnet connection)
daemon ──RFC 8030 POST, VAPID-signed, aes128gcm body──▶ push relay (Cloudflare Worker) /v1/apns/{environment}/{token}
push relay ──HTTP/2, team-key JWT, mutable-content alert carrying the ciphertext──▶ APNs ──▶ iPhone
iPhone notification service extension: decrypts the body, sets the title, body, badge and session link
```

## Decisions

### The relay speaks Web Push to the daemon

The relay is, to the daemon, one more Web Push service: the daemon posts an RFC 8291 `aes128gcm` body with an RFC 8292 VAPID `Authorization` header and the `TTL`, `Urgency` and `Topic` headers, and the relay answers with RFC 8030's statuses. The endpoint is `https://{relay}/v1/apns/{environment}/{token}` or `https://{relay}/v1/fcm/{token}`, which the daemon builds from the device's registration; the phone never sends a URL.

Reason: the daemon's sender (`push/send.rs` `send`, its `Outcome` map, the `JwtCache`, `webpush::encrypt`) and the trigger's round (`push/trigger.rs` `Target`, `send_all`, the per-audience back-off and the five-failures rule) already do exactly this and are tested against a fake push service. A relay target differs from a Web Push target only in its URL, its JWT subject and the plaintext it encrypts, so `Target` and `send_all` stay one generic path rather than gaining a second sender.

### What the relay sees: ciphertext and a fixed fallback alert

The payload is end-to-end encrypted, daemon to phone, with RFC 8291 to the phone's own `p256dh` key and `auth` secret, exactly as a browser subscription is. The relay never holds a key that opens it.

- **APNs:** the relay sends an `alert` push with `mutable-content: 1`, an alert made only of localization keys from the app's bundle (`title-loc-key`, `loc-key`; the app's strings say "Rustling Tulip" and "A session changed. Open the app to see it."), and the ciphertext base64url in a custom key `e`.

  The app's notification service extension decrypts `e`, reads the push notice, and replaces the title, body and badge and sets the session link. If it cannot decrypt in time, iOS shows the fallback alert.
- **FCM:** the relay sends a data-only message (`data.e` the ciphertext) at high priority; the app's messaging service decrypts it and posts its own notification, so Android shows no fallback text at all.

A silent (background) push was rejected for iOS: Apple throttles background pushes and drops them under load, and they never show anything if the app is not woken, so an alert could be lost silently. A visible alert with plain text from the daemon was rejected because the relay, Apple and Google would then read every session's name.

Because the fallback is made of localization keys and is the same for every push, the relay learns nothing from it, not even whether the push is a Needs you or a Finished.

The relay does see: the sender's VAPID public key (stable per daemon, so pushes of one daemon link over time), the device token and environment, the sender's IP address, the time, the body's size, and the `Topic` (`rt-` and a hash of the session id, so pushes about one session link to each other). It does not see a host name, session id, label, title, badge count or anything typed. It stores none of what it sees, apart from rate-limit counters and the deny list below.

### The push notice, defined once

The plaintext a relay push carries is a `PushNotice` in `crates/protocol` (serde JSON, so the daemon and the phone app share one definition): `v` (1), `kind` (`needs_you`, `finished`, with `#[serde(other)] Unknown`), `title`, `body` (the session's label, cut as `trigger::label` cuts it), `session_id`, `host` (the tailnet name the app paired with, from `RemoteState::Serving`; absent when the listener is not serving) and `badge` (the Needs you count).

Reason: the declarative Web Push JSON carries a web URL (`navigate`) that a native app cannot use, and the native app needs the host to know which paired daemon the session lives on. The two payloads are built from the same `Alert`, so the title, label and count rules stay in one place (`trigger::payload` beside a new `trigger::notice`).

### The phone registers with its own daemon, never with the relay

The app gets its device token from the platform, makes one P-256 key pair and 16-byte auth secret per install (kept in the Keychain in an access group the notification extension shares), and sends `RegisterPush` over its existing tailnet connection with a new registration kind:

- `apns { token, environment, p256dh, auth }`, `environment` being `production` (TestFlight and store builds) or `sandbox` (Xcode debug builds).
- `fcm { token, p256dh, auth }`.

Reason: the device is already authenticated to its daemon (its device token, its unrevoked device record, the whois owner rule), and the daemon already keys one subscription per device and drops it on block, forget and re-pair.

Registering with the relay instead would give the phone a relay credential, show the relay the phone's IP and which daemons it pairs with, and give the relay a device list to keep and lose. A phone paired to several hosts registers the same token with each; each host pushes on its own.

One key pair per install, not per host, is enough: every host the app pairs with is its owner's, and a forgotten host's subscription is deleted on that host.

### How a daemon authenticates: its VAPID key, no registration

The relay accepts a push whose VAPID JWT verifies against the `k=` public key in the same header, whose `aud` is the relay's origin and whose `exp` is within a day, exactly as RFC 8292 push services do. There is no install registration, no relay account and no daemon-side secret beyond the VAPID key the daemon already keeps in `push/vapid.json`.

Reason: a relay that served any self-hosted daemon would have to accept any registration, so a registration step would add a round trip, a relay-side table and a credential to lose, and buy no assurance a self-certifying key does not. The real abuse limits are per device token and per sender (below), not who may register.

The JWT's `sub` for a relay target is a fixed product URL (`https://github.com/leftos/rustling-tulip`), not the PWA origin the Web Push JWT carries, so the host's `*.ts.net` name never reaches the relay.

**Revocation:** the relay keeps a deny list in Workers KV of SHA-256 hashes of sender public keys and of device tokens. A denied sender is answered 403, which the daemon logs and never prunes on (as today's 403); a denied token is answered 410, which deletes that subscription. The owner adds entries with `wrangler kv key put`; there is no admin route. A daemon's owner who wants a new identity deletes the push folder, which also resets Web Push (already the documented reset).

### Rate limits and abuse

- **Per device token:** at most 20 pushes a minute and 300 a day across every sender, so a leaked token cannot be used to flood a phone, whoever sends.
- **Per sender key:** at most 120 pushes a minute.
- **Per connecting IP:** at most 600 pushes a minute, generous for many daemons behind one address but bounding a key-minting flood.
- **Size:** a body over 3072 bytes is refused with 413, which keeps the base64 ciphertext and the alert inside APNs' and FCM's 4 KB limits.
- **No open forwarding:** the relay posts only to `api.push.apple.com`, `api.sandbox.push.apple.com` and `fcm.googleapis.com`; nothing in a request names a host.
- **Kill switch:** a `RELAY_ENABLED` Worker variable; while it is off every push is answered 503 with a `Retry-After`.

The per-minute limits use the Workers Rate Limiting binding (approximate, per location); the per-token daily cap is a KV counter keyed by the token's hash and the UTC day, expiring after two days. Over a limit the relay answers 429 with a `Retry-After`, which the daemon's back-off already honours.

A leaked token can still make a phone show the fallback alert up to the per-token cap, since an iOS extension cannot drop a notification without Apple's filtering entitlement. That is accepted: device tokens are not public, and the cap bounds it.

### Who runs it and where it lives

The owner runs one relay as a Cloudflare Worker on the owner's account, at a custom domain on the owner's Cloudflare zone (chosen at deploy, PR.6), with the APNs key and the FCM service account as Worker secrets (`APNS_KEY_P8`, `APNS_KEY_ID`, `APNS_TEAM_ID`, `FCM_SERVICE_ACCOUNT`) and the bundle ID and Firebase project ID as variables. The `.p8` file and the service account JSON are kept in Private Config on G:, never in the repo.

Reasons: the owner already runs Workers (yaat's Discord bot, deployed with Wrangler); a Worker has no server to patch; Workers' production `fetch` reaches APNs over HTTP/2, which APNs requires (workerd's local `wrangler dev` does not, so local tests use a fake APNs). A custom domain rather than a `workers.dev` name means the relay can move to another Worker or account without a daemon release, since every released daemon has the origin compiled in.

The source lives in this repo at `services/push-relay/` (TypeScript, pnpm, Wrangler config, vitest on the Workers pool), not a Cargo member and never in the installer. TypeScript rather than Rust on Workers: the Workers test pool and WebCrypto ES256/RS256 are first-class there, and the relay shares no code with the daemon beyond the wire contract it tests against.

**Platform tokens:** the APNs provider JWT (ES256, team key) and the FCM OAuth access token (from the service account's RS256 assertion) are each cached in KV and reused for 50 minutes, so the Worker's many isolates do not mint a new APNs token per isolate, which Apple refuses when tokens change too often.

### What the daemon sends and how the registration grows

`PushRegistration` (`crates/protocol`) is already `kind`-tagged with a `#[serde(other)] Unknown`; it gains `Apns { token: Secret, environment: ApnsEnvironment, p256dh: String, auth: Secret }` and `Fcm { token: Secret, p256dh: String, auth: Secret }`, and `ApnsEnvironment` is `production`, `sandbox` and `#[serde(other)] Unknown`.

`PushStatus` gains an optional `device_token: Option<Secret>`, set for an `apns` or `fcm` subscription, so the native app compares and re-registers as the PWA does with `endpoint`. `Welcome.features` gains `relay_push` once the daemon sends relay pushes. All of it is additive: no protocol bump.

The daemon refuses an `apns` or `fcm` registration whose token is empty, over 512 characters, or (APNs) not hexadecimal, and one whose environment is `Unknown`, with `invalid_request`.

**On disk,** `push/subscriptions.json`'s `Subscription` becomes the switches plus a flattened, `kind`-tagged `Channel` (`web_push { endpoint, p256dh, auth }`, `apns { token, environment, p256dh, auth }`, `fcm { token, p256dh, auth }`).

A `web_push` subscription is written exactly as the released `v0.12` shape (`kind: "web_push"` beside flat `endpoint`, `p256dh`, `auth`), so released files load unchanged, and a test loads a file in that shape. `remove_if_endpoint` becomes `remove_if_address`, the address being the endpoint or the token.

**In the round,** `Target` carries the URL, the audience (the push service's origin, or the relay's), the subscriber keys and the JWT, as today, and `collect` builds a relay target's URL from `RELAY_ORIGIN` and the token. Each target names which plaintext it takes: the declarative JSON for Web Push, the `PushNotice` for the relay. Only Web Push targets need the PWA origin, so a round no longer sends nothing when the tailnet listener is not serving: it skips just the Web Push targets.

`check_endpoint` admits the relay origin besides the push services; tests reach a loopback fake relay through the `PushPolicy` and a relay origin passed into `PushState`, never by a setting or environment variable.

### Failure handling

The relay answers with RFC 8030's statuses, so the daemon's existing `Outcome` map applies unchanged:

| Relay answer | Cause | Daemon |
|---|---|---|
| 201 | APNs or FCM accepted | `Sent` |
| 410 | APNs `Unregistered`, `BadDeviceToken` or `DeviceTokenNotForTopic`; FCM `UNREGISTERED` or a token `INVALID_ARGUMENT`; a denied token | `Gone`: the subscription is deleted, as a Web Push 410 |
| 403 | the VAPID JWT does not verify, or the sender is denied | kept, logged |
| 413 | body over the size cap, or the platform's `PayloadTooLarge` | logged with its size |
| 429 | a relay limit, or the platform's `TooManyRequests` | back-off with `Retry-After` |
| 502, 503 | the platform failed or was unreachable, the relay's own platform token was refused, or the kill switch is on | back-off |

The relay's own credential failures (APNs `InvalidProviderToken`, `ExpiredProviderToken`, an FCM 401) answer 502, never 403, so a daemon never mistakes the relay's fault for its own; the relay logs them.

When the relay is down or unreachable, the relay's origin is backed off like any push service (30 seconds doubling to ten minutes), and a push that was not taken is not queued, as with Web Push today: a stale alert is worse than none, the platform's own `TTL` covers a phone that is offline, and the app shows every session's state when it opens.

### Opt-in and privacy

The daemon contacts the relay only for an `apns` or `fcm` subscription, which exists only after the phone's user turned notifications on in the app (a direct user action), and stops when the subscription is removed. No other daemon data goes to the relay, and the relay's logs carry the status, the platform and a hash prefix of the sender key, never a token or a body. Nothing here runs an agent or calls a paid provider.

### What stays out

- **Any inbound path.** The relay never connects to a daemon, and a daemon never listens for it; exposing the daemon to the internet or reaching it through an inbound relay stays a non-goal ([architecture.md](../architecture.md#non-goals)).
- **The PWA.** It keeps direct Web Push with no relay; the web app is frozen.
- **Push for a phone reached over SSH.** Push registration is keyed by a tailnet device record, and an SSH bridge connection is `Local` (`server/connection.rs` `AuthKind`), which the push handler refuses. The native app requires the Tailscale app and registers over its tailnet connection, so a phone that reaches the PC over SSH adds the PC as a tailnet host too for push; SSH connections get no device identity (`docs/plans/mobile-app.md`, the push decision).
- **UnifiedPush and other push services.** Android waits on the GPUI spike's verdict; when it starts, FCM through the relay is the path, since UnifiedPush needs a distributor app on the phone.
- **Delivery receipts, notification actions and a relay dashboard.** None is needed to deliver an alert.

## Contract for the native app (RT-1266)

These are the native app's steps, named here so the relay's steps can wait on them:

- **The native app's push registration step:** on the Notifications switch, ask for notification permission, register for remote notifications, make or load the install's P-256 key pair and auth secret in the shared Keychain access group, and send `RegisterPush` with the `apns` kind and the build's environment over the tailnet connection; on each open compare `PushStatus.device_token` with the current token and re-register when they differ.
- **The native app's notification extension step:** a notification service extension that reads `e`, decrypts it with `push-crypto`'s RFC 8291 decrypt (linked from Rust, so encryption and decryption are proved by one test vector in one crate), parses the `PushNotice`, sets the title, body and badge, and puts `host` and `session_id` in the notification's user info; a tap opens that session on that host. On any failure it leaves the fallback alert as it came.
- **The native app's signing step:** the app's bundle ID with the Push Notifications capability and the `aps-environment` entitlement, which the relay's APNs topic names.

## Steps

Each step is one brief. Daemon steps that touch `crates/protocol/src/lib.rs` run one at a time.

- [ ] **PR.1 Share the RFC 8291 code as a `push-crypto` crate with decrypt.** Move `encrypt` and `Subscriber` from `crates/daemon/src/push/webpush.rs` into a new workspace crate `crates/push-crypto` (no daemon, tokio or GPUI dependency, builds for iOS targets), add the `decrypt` the tests already carry as public API, and have the daemon call it.
  - Files: `Cargo.toml`, `crates/push-crypto/Cargo.toml`, `crates/push-crypto/src/lib.rs`, `crates/daemon/Cargo.toml`, `crates/daemon/src/push/webpush.rs`, `crates/daemon/src/push/trigger.rs`.
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr1-crypto.log --timeout 1800 --slot heavy -- cargo nextest run -p push-crypto --cargo-profile test-release`, then the same with `-p daemon --cargo-profile test-release push::` (the RFC 8291 Appendix A vector both ways, a round trip, and the daemon's push tests unchanged).
  - Waits on: nothing. Needs the owner: no.
- [x] **PR.2 Add the `apns` and `fcm` registrations and the push notice to the wire.** `PushRegistration::Apns` and `Fcm`, `ApnsEnvironment`, `PushNotice` with its `kind` enum, `PushStatus.device_token`, and the `FEATURE_RELAY_PUSH` constant (`relay_push`, not yet advertised); regenerate `apps/pwa/src/protocol.ts` with `cargo run -p protocol-ts`.
  - Files: `crates/protocol/src/lib.rs`, `apps/pwa/src/protocol.ts`, `docs/wire-protocol.md`.
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr2-wire.log --timeout 1800 --slot heavy -- cargo nextest run -p protocol --cargo-profile test-release push` (round trips, an unknown environment and an unknown notice kind decode as `Unknown`, a `PushStatus` without `device_token` decodes), then `-p protocol-ts --cargo-profile test-release generated_protocol_ts_is_current`.
  - Waits on: nothing. Needs the owner: no.
- [ ] **PR.3 Store a subscription as switches plus a `kind`-tagged channel.** `Subscription` becomes the switches and a flattened `Channel`; `remove_if_endpoint` becomes `remove_if_address`; the callers in `state.rs`, `trigger.rs` and `server/push.rs` follow.
  - Files: `crates/daemon/src/push/store.rs`, `crates/daemon/src/push/state.rs`, `crates/daemon/src/push/trigger.rs`, `crates/daemon/src/server/push.rs`, `docs/on-disk.md`.
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr3-store.log --timeout 1800 --slot heavy -- cargo nextest run -p daemon --cargo-profile test-release push::` (a `subscriptions.json` in the `v0.12` shape loads and saves back in the same shape, an `apns` subscription round trips, a late gone for an old token never deletes a new one, `Debug` redacts the token).
  - Waits on: PR.2. Needs the owner: no.
- [ ] **PR.4 Accept `apns` and `fcm` registrations from a paired phone.** The handler checks the token and environment, stores the channel, and fills `PushStatus.device_token`.
  - Files: `crates/daemon/src/server/push.rs`, `crates/daemon/src/push/store.rs`.
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr4-handler.log --timeout 1800 --slot heavy -- cargo nextest run -p daemon --cargo-profile test-release server::push` (stored and answered, a replacement token replaces, an empty, overlong or non-hex APNs token and an `Unknown` environment are `invalid_request` with no token in the message, a loopback connection is still refused).
  - Waits on: PR.3. Needs the owner: no.
- [ ] **PR.5 Build the push relay Worker with its APNs sender.** `services/push-relay/`: the RFC 8030 front (VAPID verification against `k=`, `aud` and `exp` checks, `aes128gcm` and size checks), the deny list, the rate limits and daily cap, the kill switch, the APNs sender (cached provider token, `mutable-content` alert with the localization keys and `e`, `apns-collapse-id` from `Topic`, `apns-expiration` from `TTL`, priority from `Urgency`), the status map, and a `GET /health`.

  Tests run on the Workers vitest pool with outbound `fetch` mocked as APNs and the clock passed in.
  - Files: `services/push-relay/package.json`, `services/push-relay/wrangler.jsonc`, `services/push-relay/src/index.ts`, `services/push-relay/src/vapid.ts`, `services/push-relay/src/apns.ts`, `services/push-relay/test/relay.test.ts`.
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr5-relay.log --timeout 1200 --slot light -- pnpm -C services/push-relay install --frozen-lockfile`, then the same gate with `-- pnpm -C services/push-relay test`.

    It shows: a JWT minted by the daemon's `vapid::sign_jwt`, checked in as a fixture, verifies at its signing time; a wrong `aud`, an expired JWT and a denied key are 403; a body over the cap is 413; each APNs answer maps as the table says; the per-token limit answers 429 with `Retry-After`; no request reaches a host but APNs.
  - Waits on: nothing. Needs the owner: no.
- [ ] **PR.6 Deploy the relay.** Create the KV namespaces and the rate-limit bindings, set the secrets and variables, attach the custom domain, deploy, and write the runbook (deploy, rotate the APNs key, deny a sender or token, the kill switch) in `services/push-relay/README.md`; record the origin for PR.7.
  - Files: `services/push-relay/wrangler.jsonc`, `services/push-relay/README.md`, `CLAUDE.md` (the project-shape line for `services/push-relay/`), `docs/README.md` (glossary: push relay).
  - Proof: the relay's suite as in PR.5, then `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr6-health.log --timeout 120 --slot none -- curl -sf https://{relay host}/health`.
  - Waits on: PR.5; the native app's signing step (the bundle ID the APNs topic names). Needs the owner: yes: an APNs auth key (`.p8`) with APNs enabled in the Apple Developer account, the push capability on the app's bundle ID, the hostname on the owner's Cloudflare zone, and `wrangler login` on the owner's Cloudflare account; the `.p8` goes to Private Config on G:.
- [ ] **PR.7 Send relay pushes from the daemon.** `push/relay.rs` holds `RELAY_ORIGIN`, the relay URL from a channel, and the fixed JWT subject; `check_endpoint` admits the relay origin; `trigger::notice` builds the `PushNotice`; `collect` and `send_all` give each target its plaintext, the origin gate applies to Web Push targets only; the fake push service doubles as a fake relay; `Welcome.features` lists `relay_push`.
  - Files: `crates/daemon/src/push/relay.rs`, `crates/daemon/src/push/send.rs`, `crates/daemon/src/push/trigger.rs`, `crates/daemon/src/push/test_fake.rs`, `crates/daemon/src/server/handshake.rs`, `docs/session-runtime.md` ("Phone pushes", and the glossary's push notice entry in `docs/README.md` in the same commit).
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr7-daemon-relay.log --timeout 1800 --slot heavy -- cargo nextest run -p daemon --cargo-profile test-release push::`.

    It shows: an `apns` subscription posts to `/v1/apns/production/{token}` on the fake relay with the VAPID headers and a JWT whose `sub` is the product URL; the body decrypts with `push-crypto` to the `PushNotice`; a relay 410 deletes the subscription; a 503 backs the relay off without touching Web Push targets; with the listener not serving, the relay target is still pushed and the Web Push target is not; `Welcome.features` lists `relay_push`.
  - Waits on: PR.1, PR.4, PR.6 (the origin). Needs the owner: no.
- [ ] **PR.8 Prove a relay push on a real iPhone.** A section in `docs/remote-hand-checklist.md`: a TestFlight build registers, a Needs you arrives locked and off the tailnet with the session's name (decrypted), the tap opens that session on that host, the badge matches, the fallback alert shows when the extension is made to fail, a deleted app's token is pruned after its 410, and the relay's logs carry no token or body.
  - Files: `docs/remote-hand-checklist.md`, `docs/plans/mobile-app.md` (Push: the native path's line).
  - Proof: the hand checklist on the owner's iPhone, with the daemon log's push lines and the relay's `wrangler tail` output attached to the issue.
  - Waits on: PR.7; the native app's push registration step; the native app's notification extension step. Needs the owner: yes: a real iPhone with the TestFlight build.
- [ ] **PR.9 Add the FCM sender to the relay.** The FCM HTTP v1 sender (service-account assertion, cached access token, data-only high-priority message with `e`, `collapse_key` from `Topic`, `ttl` from `TTL`), its status map, and the Firebase secrets in the runbook.
  - Files: `services/push-relay/src/fcm.ts`, `services/push-relay/src/index.ts`, `services/push-relay/test/relay.test.ts`, `services/push-relay/README.md`.
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/pr9-fcm.log --timeout 1200 --slot light -- pnpm -C services/push-relay test` (an `fcm` push posts a data-only message to the mocked FCM; `UNREGISTERED` is 410; an FCM 401 is 502).
  - Waits on: PR.5; the native app's Android phase starting, since the GPUI spike says wait on Android and a Firebase project is only worth creating then. Needs the owner: yes: a Firebase project and a service account with the FCM sender role, kept in Private Config on G:.
