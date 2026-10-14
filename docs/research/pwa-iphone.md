# PWA on the owner's iPhone: platform facts

Researched 2026-10-13, for re-planning [mobile-app.md](../plans/mobile-app.md) after RT-38 (PWA on iPhone, served by the daemon over Tailscale HTTPS, Web Push with the owner's own VAPID keys).

Compatibility rows marked "MDN BCD" are from the raw data files of [mdn/browser-compat-data](https://github.com/mdn/browser-compat-data) (`main`, fetched on the date above); `safari_ios` there mirrors desktop Safari unless it says otherwise. "Unverified" means no primary source was found in this pass.

## 1. Web Push on iOS

- **Minimum version: iOS/iPadOS 16.4, Home Screen web apps only** (a manifest with `display: standalone` or `fullscreen`, added via Share, Add to Home Screen). Safari tabs on iOS get no push.

  Source: [WebKit, Web Push for Web Apps on iOS and iPadOS](https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/) (iOS 16.4); [Apple, Sending web push notifications](https://developer.apple.com/documentation/usernotifications/sending-web-push-notifications-in-web-apps-and-browsers) (iOS 16.4+). MDN BCD `api.PushManager`: safari_ios 16.4, note "supported in web apps saved to the home screen".
- **Permission flow:** the request must come from a direct user interaction (a tap on a subscribe button), in the installed app; iOS then shows its prompt, and the permission is managed per web app in Settings, Notifications, like a native app. Apple: "call the push subscription method immediately from the gesture's event handler code". Sources: WebKit blog above (16.4); Apple doc above.
- **No Apple Developer account needed**; the push service is APNs at `*.push.apple.com`, which the sender (the daemon) must be able to reach. Source: WebKit blog above (16.4).
- **No silent pushes:** subscriptions must set `userVisibleOnly: true`, and a service worker that does not show a notification for a push gets its subscription revoked. Apple: "If you don't, Safari revokes the push notification permission for your site." Sources: Apple doc above; [WebKit, Meet Declarative Web Push](https://webkit.org/blog/16535/meet-declarative-web-push/) (all WebKit Web Push). How many misses Apple tolerates before revoking is unverified.
- **Payload limit: 4 KB** (APNs answers 413 "The payload size is over the limit of 4 KB"). 

  Other responses Apple documents: 403 for a bad VAPID JWT (wrong key, subject not a URL or `mailto:`, audience not the push service origin, `exp` more than a day ahead), 410 "The device token has expired", 429 too many requests. Apple asks senders not to refresh the JWT more than once an hour. Source: Apple doc above.
- **Delivery while closed or locked:** notifications behave like native ones: Lock Screen, Notification Center, paired Apple Watch, and they obey Focus (Focus settings follow the manifest `id` plus the name given at install, and sync across the owner's devices). Source: WebKit blog above (16.4).

  No Apple figure for latency or throttling was found; delivery reliability beyond "works like native" is unverified.
- **Known issues:**
  - Subscriptions can change or vanish: [WebKit bug 273063](https://bugs.webkit.org/show_bug.cgi?id=273063) "webPush subscription becomes invalid for few users" is open (NEW, last changed 2024-07-30).

    An Apple forum report says `getSubscription()` returns null after relaunch while pushes still arrive, and re-subscribing duplicates them ([forum 769794](https://developer.apple.com/forums/thread/769794)).

    Others report pushes stopping after a few messages, fixed by reinstalling ([forum 728796](https://origin-devforums.apple.com/forums/thread/728796)). iOS version per report unverified.
  - Intelligent Tracking Prevention deletes all website data, service worker registrations included, for sites not visited in a while, which leaves a classic push subscription useless; WebKit gives this as a motivation for Declarative Web Push. Source: Meet Declarative Web Push (above). Whether ITP's timer applies to an installed Home Screen web app, and after how long, is unverified.
  - A notification tap that launches the app could land in a window that was briefly inert: [WebKit bug 252544](https://bugs.webkit.org/show_bug.cgi?id=252544), RESOLVED FIXED 2024-08-03 (the iOS release carrying the fix is unverified).
  - Apple mints a new endpoint each time the service worker re-registers (reinstall, reset), and stale ones do not always answer 410: herdr collie saw 20 dead `web.push.apple.com` endpoints for one iPhone ([collie #104](https://github.com/AltanS/collie/issues/104)), and Apple answers some dead tokens with 400 `BadDeviceToken` rather than 410 ([collie #68](https://github.com/AltanS/collie/issues/68), [bridge/push.ts](https://github.com/AltanS/collie/blob/HEAD/bridge/push.ts)).

    The daemon should dedupe on subscribe (per device) and prune on 404/410 plus repeated permanent failures.
  - APNs decodes the RFC 8030 `Topic` header as base64url and rejects a topic whose length base64 cannot produce (length ≡ 1 mod 4), for example 13 characters; FCM and Mozilla treat it as opaque. Source: collie [bridge/push.ts](https://github.com/AltanS/collie/blob/HEAD/bridge/push.ts) comments (observed behaviour, not Apple documentation).
- **Declarative Web Push (iOS/iPadOS 18.4, macOS 15.5):** a push whose JSON body is `{"web_push": 8030, "notification": {"title": …, "navigate": <url>, "app_badge": "1", …}}` is shown by the OS with no service worker; `window.pushManager` can subscribe with no service worker; a service worker, if present, may rewrite the notification, and if it fails the declarative message is the fallback, with no revocation penalty. Removing the service worker does not drop the subscription.

  Sources: [Meet Declarative Web Push](https://webkit.org/blog/16535/meet-declarative-web-push/); [Safari 18.4 release notes](https://developer.apple.com/documentation/safari-release-notes/safari-18_4-release-notes) ("Added support for Declarative Web Push").

  For this app it removes the silent-push revocation risk and the ITP service-worker loss, and sets the badge from the payload. Browsers without it handle the same JSON in a normal `push` handler (blog, backwards compatibility section).
- **Android Chrome:** Web Push works in the browser tab too, not only when installed; Chrome has required `userVisibleOnly: true` since push shipped (Chrome 42, MDN BCD `api.PushManager`). Declarative Web Push support in Chrome is unverified.

## 2. Badging API

- `navigator.setAppBadge(n)` / `clearAppBadge()` work in iOS/iPadOS 16.4+ Home Screen web apps, both in the foreground and **while handling push events in the background**, even before badge permission is granted. Source: [WebKit 16.4 blog](https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/). MDN BCD `api.Navigator.setAppBadge`: safari_ios 16.4, "supported for web apps saved to the home screen"; `setAppBadge(0)` clears the badge.
- **Permission:** the badge shows once the user allows notifications; it is then configurable in Settings, Notifications. Sources: WebKit 16.4 blog; Apple doc above.
- With Declarative Web Push (18.4) the payload's `app_badge` sets it with no script. Source: Meet Declarative Web Push.
- MDN BCD has no `WorkerNavigator.setAppBadge` entry, so the service-worker case rests on the WebKit blog statement above.
- **Android Chrome:** MDN BCD lists `clearAppBadge` as chrome_android 81 and gives no chrome_android support for `setAppBadge`; Android launchers show their own notification dot. Treat the badge as iOS-only.

## 3. Notification actions and click routing

- **Action buttons: not supported in Safari** (MDN BCD `api.Notification.actions`: safari has no `version_added`, safari_ios mirrors it). Chrome supports them (chrome 53, chrome_android mirrors). So iOS needs the tap to open the app at a reply view; Android could later offer inline buttons.
- **Click routing:** a `notificationclick` handler can `clients.matchAll` then `focus`/`navigate` an existing window or `clients.openWindow(url)`; collie does this to open the agent that alerted ([web/src/sw.ts](https://github.com/AltanS/collie/blob/HEAD/web/src/sw.ts)).

  Collie notes that `openWindow` needs the click's user activation, so the notification is closed only after the open succeeds. With Declarative Web Push the `navigate` URL does it without script. Behaviour on iOS for the warm-app case had a bug, now fixed (bug 252544 above). Detailed iOS verification is unverified.

## 4. Other capabilities the plan touches

| Capability | iOS | Android Chrome | Source |
| --- | --- | --- | --- |
| Web Share Target (manifest `share_target`) | Not supported | chrome_android 76 | MDN BCD `manifests.webapp.share_target` |
| Web Share (`navigator.share`, outbound) | Safari 12.1 (mirrored) | chrome_android 61 | MDN BCD `api.Navigator.share` |
| `SpeechRecognition` | `webkitSpeechRecognition`, Safari 14.1 (mirrored), needs Siri/Dictation on; reported broken in Home Screen web apps | Chrome (unprefixed from 139) | MDN BCD `api.SpeechRecognition`; [WebKit bug 225298](https://bugs.webkit.org/show_bug.cgi?id=225298) (RESOLVED LATER); [Apple forum 748048](https://developer.apple.com/forums/thread/748048) |
| `navigator.vibrate` | Not supported | chrome_android 32; needs a user gesture since Chrome 60 | MDN BCD `api.Navigator.vibrate` |
| Screen Wake Lock | iOS 16.4 announced; fixed for Home Screen web apps in 18.4 | Chrome 84 | [WebKit 16.4 blog](https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/); [Safari 18.4 notes](https://developer.apple.com/documentation/safari-release-notes/safari-18_4-release-notes) ("Fixed Wake Lock API for Home Screen Web Apps"); MDN BCD `api.WakeLock` (safari_ios 18.4) |
| `navigator.storage.persist()` | iOS 17 / Safari 17; granted by heuristics "like whether the website is opened as a Home Screen Web App" | Chrome 55 | [WebKit, Updates to Storage Policy](https://webkit.org/blog/14403/updates-to-storage-policy/) |

- **Voice input:** since the Web Speech API is unreliable in a standalone iOS PWA, record audio (`MediaRecorder`) and transcribe on the daemon, as collie does (its iOS recorder output is AAC in MP4; [docs/voice-and-push.md](https://github.com/AltanS/collie/blob/HEAD/docs/voice-and-push.md)). iOS dictation from the keyboard's microphone key works in any text field (general iOS behaviour; not checked against a source here).
- **Storage quota and eviction (iOS 17+):** origin quota up to 15% of disk for non-browser apps, 60% for browsers; overall 20%/80%; a standalone web app gets the same quota as in the browser. Eviction happens over the overall quota, under storage pressure, or after no user interaction for some time (ITP), least recently used first; an origin with an active page or in persistent mode may be excluded.

  Source: Updates to Storage Policy (above).
- **The 7-day cap:** WebKit's ITP deletes script-writable storage for sites without user interaction in 7 days of Safari use, and exempts Home Screen web apps, which have their own counter of days of use ([WebKit, Full Third-Party Cookie Blocking and More](https://webkit.org/blog/10218/full-third-party-cookie-blocking-and-more/), iOS 13.4 / Safari 13.1). That post was not re-fetched in this pass, so the exact wording is unverified here; re-read it before relying on it.
- **WebAuthn / passkeys in a Home Screen web app:** unverified in this pass. Not needed if the daemon keeps device pairing tokens (as collie does) behind the tailnet.
- **WebSocket when backgrounded:** iOS suspends a backgrounded web app; no Apple document giving a time was found, so how fast it drops is unverified. Plan for the socket to die whenever the app leaves the foreground, reconnect and resync on `visibilitychange`, and rely on push for alerts.

## 5. Tailscale HTTPS

- **Setup:** enable MagicDNS and HTTPS Certificates in the admin console; then `tailscale cert` gets a Let's Encrypt certificate (DNS-01 via a `*.ts.net` TXT record Tailscale creates) for `machine.tailNNNN.ts.net`. The private keys stay on the machine.

  The machine name is published in Certificate Transparency logs, so pick a non-revealing name. A bare `https://machine-name` cannot have a certificate. Source: [Tailscale, Enabling HTTPS](https://tailscale.com/kb/1153/enabling-https) (validated 2025-12-10).
- **Renewal:** certificates last 90 days. Files written by `tailscale cert` are the user's to renew; certificates tailscaled handles itself (Serve, the Caddy integration, Go's `local.Client.GetCertificate`) renew automatically. Repeated requests can hit Let's Encrypt rate limits (a wait of up to 34 hours). Source: Enabling HTTPS.
- **`tailscale serve`:** `tailscale serve --bg <port>` terminates HTTPS on :443 with the auto-provisioned certificate and proxies to `http://127.0.0.1:<port>` (only 127.0.0.1 proxies are supported).

  It injects `Tailscale-User-Login` / `-Name` / `-Profile-Pic` and strips client-sent copies, so the backend should listen on localhost only. Tagged devices get no identity headers. Never use `funnel` for this, which makes the port public.

  Sources: [Tailscale Serve](https://tailscale.com/kb/1312/serve) (validated 2026-01-20); [tailscale serve command](https://tailscale.com/kb/1242/tailscale-serve) (validated 2026-01-26). Serve's HTTPS mode on Windows is documented without platform caveats, but collie lists it as not yet tested on Windows (README, Windows section).
- **iOS trust:** the certificate is from Let's Encrypt, a public CA, so Safari trusts it with no profile to install (follows from Enabling HTTPS; not separately tested here). The iPhone must be on the tailnet (Tailscale app connected) to reach the `*.ts.net` address.
- **Secure context, service worker and push on `*.ts.net`:** an HTTPS origin with a valid certificate is a secure context, which is what service workers and Push require; collie runs exactly this (Web Push over `tailscale serve`, documented in [voice-and-push.md](https://github.com/AltanS/collie/blob/HEAD/docs/voice-and-push.md)).
- **Receiving pushes off the tailnet:** the daemon POSTs to the endpoint `web.push.apple.com` gave the phone, over the public internet, and APNs delivers to the phone; the tailnet is not in that path, so the phone need not be connected to Tailscale to receive a push, only to open the app's data afterwards.

  This follows from the Web Push design (WebKit 16.4 blog: Web Push uses APNs) and is not stated by Tailscale or Apple in these words. The Windows machine needs outbound HTTPS to `*.push.apple.com`.

## 6. Prior art: herdr collie

[github.com/AltanS/collie](https://github.com/AltanS/collie) (MIT), "ColliePWA", a self-hosted PWA mobile client for Herdr (and experimental tmux/zellij), read from its default branch on the date above.

- **Serving and TLS:** "served over Tailscale by default"; Variant A is `tailscale serve --bg 8787` (HTTPS :443 to 127.0.0.1:8787), and the bridge trusts `Tailscale-User-Login` when `COLLIE_TRUSTED_USER` is set; Variant C puts a reverse proxy in front with no Tailscale. On Windows collie does not run `tailscale serve` itself. Sources: [README](https://github.com/AltanS/collie/blob/HEAD/README.md); [docs/install.md](https://github.com/AltanS/collie/blob/HEAD/docs/install.md).
- **Auth:** device pairing is always on: `collie pair` shows a QR code; a device needs its token for every request, and an unpaired browser sees only the pair screen (README; install.md). Actions are plain HTTP POSTs (`/api/pane/:id/{reply,keys}`).
- **Install:** Safari, Share, Add to Home Screen; no store listing (install.md, "iPhone and iPad").
- **Push:** the Node `web-push` library (3.6.x, optional dependency in [package.json](https://github.com/AltanS/collie/blob/HEAD/package.json)); `collie push-keys [mailto:…]` generates VAPID keys into `.env`; replacing keys invalidates every subscription.

  Alerts fire on blocked/done states; a tap opens that agent. Push titles travel as codes the phone translates ([ADR 0074](https://github.com/AltanS/collie/blob/HEAD/.adr/0074-a-push-title-is-a-code-the-phone-translates.md)). It uses a short base64-valid `Topic` as a collapse key so a reconnecting phone gets the latest message only.

  Source: [bridge/push.ts](https://github.com/AltanS/collie/blob/HEAD/bridge/push.ts); [docs/voice-and-push.md](https://github.com/AltanS/collie/blob/HEAD/docs/voice-and-push.md).
- **Service worker:** hand-written on Workbox precaching, with `push` and `notificationclick` handlers ([web/src/sw.ts](https://github.com/AltanS/collie/blob/HEAD/web/src/sw.ts)); it sets a notification `badge` image for Android's status bar. A code search found no `setAppBadge` call in the repo.
- **Terminal renderer:** none. Collie reads Herdr's already-rendered screen grid and draws it as React text nodes with an SGR parser; [ADR 0008](https://github.com/AltanS/collie/blob/HEAD/.adr/0008-collie-does-not-run-a-terminal-emulator.md) rejects running a terminal emulator.

  rustling-tulip's daemon has raw PTY bytes, so it would need an emulator (in the page, or headless in the daemon); collie does not settle that choice for us.
- **iOS issues it reports:** stale Apple endpoints piling up (#104), dead tokens answered with 400 `BadDeviceToken` (#68), the base64 topic rule above; voice input "on an iPhone is not verified" (voice-and-push.md). No badge issue was found.
