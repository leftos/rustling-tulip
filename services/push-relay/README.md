# Push relay

The push relay is the Cloudflare Worker that, to a daemon, is one more RFC 8030 Web Push service: it takes a VAPID-signed, `aes128gcm`-encrypted push for a native phone and forwards it to APNs as a mutable-content alert carrying the ciphertext (or, when the daemon marks it, as a background push that shows nothing), so only the relay holds the Apple key. One relay serves every daemon. The design is in [docs/plans/push-relay.md](../../docs/plans/push-relay.md).

## Where it runs

The relay is the `rustling-tulip-push-relay` Worker on the owner's Cloudflare account, served at the custom domain `https://rt-push.leftos.dev`, with `workers_dev` off so it has no `workers.dev` name. `GET /health` answers `ok`. Its KV namespaces, rate-limit bindings and variables are in `wrangler.jsonc`.

## Request contract

A daemon posts a push to `/v1/apns/<production|sandbox>/<device token>`, signed with VAPID and encrypted `aes128gcm`; `TTL` sets `apns-expiration`, `Urgency: high` sets priority 10 and `Topic` sets `apns-collapse-id`. A push with no `X-RT-Push-Type` header, or an empty one, is the mutable-content alert. `X-RT-Push-Type: background` (any case, spaces trimmed) sends it as an APNs background push instead: `apns-push-type: background`, always priority 5, and a body of only `{"aps":{"content-available":1},"e":"<ciphertext>"}`, which wakes the app without showing anything. Any other value is answered `400 push type is not background` before the push counts against the rate limits or the daily cap. Deploy the relay before any daemon that sends the header: an older relay ignores it and delivers the push as an alert with nothing to show.

## Rate limits

Each device token gets two budgets, one for alerts and one for background pushes, so a background push never spends an alert's. Each kind has its own per-device bucket of 20 a minute (`TOKEN_LIMITER`, keyed by the token's hash for an alert and `bg:` plus the hash for a background push; over it, `429` with `Retry-After: 60`) and its own daily cap of 300 per UTC day (the `RELAY_STATE` counters `day:<hash>:<YYYY-MM-DD>` and `daybg:<hash>:<YYYY-MM-DD>`; over it, `429 daily cap for this device reached` with `Retry-After` the seconds to the next UTC midnight). The per-IP and per-sender limits (`IP_LIMITER`, `SENDER_LIMITER`) are shared by both kinds.

## Deploy

From this folder:

```bash
pnpm install
pnpm test
pnpm exec wrangler deploy
```

`pnpm exec wrangler deploy` uploads the Worker and attaches the custom domain; the tests use their own bindings, never the deployed ones.

## Secrets

The Worker has three secrets: `APNS_KEY_P8` (the APNs auth key `.p8` file), `APNS_KEY_ID` and `APNS_TEAM_ID`. They come from the push-relay folder in the owner's Private Config on G: (the `AuthKey_*.p8` file and `ids.env`), never from the repo, and are set with `wrangler secret put` reading stdin, so no value is printed or passed on a command line.

From this folder, in Git Bash, with `S` set to the push-relay folder in the owner's Private Config:

```bash
P8=$(ls "$S"/AuthKey_*.p8)
pnpm exec wrangler secret put APNS_KEY_P8 < "$P8"
bash -c 'set -a; . "$0/ids.env"; printf %s "$APNS_KEY_ID" | pnpm exec wrangler secret put APNS_KEY_ID' "$S"
bash -c 'set -a; . "$0/ids.env"; printf %s "$APNS_TEAM_ID" | pnpm exec wrangler secret put APNS_TEAM_ID' "$S"
```

Never read, print or echo either file or a variable loaded from it.

## Rotate the APNs key

1. Make a new key in the Apple Developer account and save its `.p8` file to the push-relay folder in Private Config, updating `APNS_KEY_ID` in `ids.env`.
2. Set `APNS_KEY_P8` and `APNS_KEY_ID` with `wrangler secret put` as in [Secrets](#secrets).
3. Delete the cached provider token so the next push mints one with the new key: `pnpm exec wrangler kv key delete --namespace-id <RELAY_STATE id> apns-jwt --remote`, the id being `RELAY_STATE`'s in `wrangler.jsonc`. Skipping it costs one refused push: the relay drops the cached token itself when APNs answers `InvalidProviderToken` or `ExpiredProviderToken`.
4. Revoke the old key in the Apple Developer account.

## Deny a sender or a token

The `DENY_LIST` namespace holds the deny entries; any value denies. A denied sender is answered 403, a denied device token 410.

- A sender: `pnpm exec wrangler kv key put --namespace-id <DENY_LIST id> sender:<hex> 1 --remote`, where `<hex>` is the lowercase SHA-256 hex of the raw (decoded) `k=` public key point from the sender's VAPID `Authorization` header.
- A device token: `pnpm exec wrangler kv key put --namespace-id <DENY_LIST id> token:<hex> 1 --remote`, where `<hex>` is the SHA-256 hex of the token in lowercase hex.

The token's hash comes from `printf %s <token in lowercase> | sha256sum`. The sender's comes from the `k=` value decoded from base64url to bytes first, for example `printf %s <k value> | tr '_-' '/+' | base64 -d 2>/dev/null | sha256sum` (add the `=` padding base64 needs if it complains). The relay logs a push's sender as the first 8 characters of this hex.

Deleting the key lifts the denial: `pnpm exec wrangler kv key delete --namespace-id <DENY_LIST id> <key> --remote`.

## Kill switch

Set `RELAY_ENABLED` in `wrangler.jsonc` to anything but `"true"` and deploy, or change the variable in the Cloudflare dashboard. While it is off the relay answers every request 503 with a `Retry-After`; set it back to `"true"` to serve again.

## Logs

`pnpm exec wrangler tail` streams the Worker's logs. Each push logs one JSON line with its status, the platform and the sender's hash prefix, never a token, body or header value.
