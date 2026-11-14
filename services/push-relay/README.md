# Push relay

The push relay is the Cloudflare Worker that, to a daemon, is one more RFC 8030 Web Push service: it takes a VAPID-signed, `aes128gcm`-encrypted push for a native phone and forwards it to APNs as a mutable-content alert carrying the ciphertext, so only the relay holds the Apple key. One relay serves every daemon. The design is in [docs/plans/push-relay.md](../../docs/plans/push-relay.md).

## Where it runs

The relay is the `rustling-tulip-push-relay` Worker on the owner's Cloudflare account, served at the custom domain `https://rt-push.leftos.dev`, with `workers_dev` off so it has no `workers.dev` name. `GET /health` answers `ok`. Its KV namespaces, rate-limit bindings and variables are in `wrangler.jsonc`.

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
3. Delete the cached provider token so the next push mints one with the new key: `pnpm exec wrangler kv key delete --namespace-id <RELAY_STATE id> apns-jwt --remote`, the id being `RELAY_STATE`'s in `wrangler.jsonc`.
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
