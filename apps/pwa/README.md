# rustling-tulip phone app

The web app a phone opens over the tailnet: the installable shell the daemon serves from its `pwa` folder (`crates/daemon/src/server/pwa_static.rs`), built with Vite and plain TypeScript. The service worker in `src/sw.ts` is hand-written; there are no runtime dependencies.

## Scripts

Run from this folder, or from the repo root as `pnpm --dir apps/pwa <script>`:

| script              | what it does                                                                                                                  |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `dev`               | Vite dev server                                                                                                               |
| `build`             | type-check both projects, then write `dist/`                                                                                  |
| `typecheck`         | `tsc --noEmit` for the page, then for the service worker (`tsconfig.sw.json`, which type-checks it against the WebWorker lib) |
| `lint`              | oxlint, with the typescript, import and unicorn plugins                                                                       |
| `fmt` / `fmt:check` | oxfmt, rewrite or check                                                                                                       |
| `test`              | vitest, one pass                                                                                                              |

## Protocol types

`src/protocol.ts` is generated from `crates/protocol` and is never edited by hand; oxlint and oxfmt skip it. After a protocol change, regenerate it from the repo root with `cargo run -p protocol-ts` (through `tools/gate.ps1 run`) and commit the result: the `generated_protocol_ts_is_current` test in `tools/protocol-ts` fails while the file is stale. A switch over a tag keeps a default branch, since a newer daemon can send tags the file does not list.

## Connection

The page opens one WebSocket to `wss://{host}/ws` (its own origin) on load and sends `Hello` with its stored device token, `protocol_version` and `protocol_versions`, and no `client_id`; there is no UI yet, and the state shows only in the console (`pwa connection {kind: "status", state}`, `pwa pairing <model>`).

An unpaired phone gets `Welcome` then `DevicePairFailed { reason: "device not approved" }`, and the socket stays open as the pairing channel (state `pairing`); nothing redials by itself there. `Connection.askToPair()` sends the request (redialling first when the channel has closed), `Connection.redeem(code)` sends the six-digit code once the owner approves, and the `device_token` reply is stored and the page redials with it. A stored token the daemon refuses with `device not approved` is forgotten and the phone falls back to pairing; `Connection.forget()` does the same on demand. Any other refusal of a token stops at `notApproved`.

- `src/connection-machine.ts`: a pure reducer over the states idle, connecting, handshaking, ready, backoff, hidden, pairing and notApproved.
- `src/pairing.ts`: a pure reducer over the pairing steps (asking, pending, approved, denied, paired) and the error text the screen shows.
- `src/socket.ts`: the driver, with the socket, timers, visibility and token store injected. A Welcome that does not arrive in 10 s is a failure; backgrounding closes the socket and clears timers; returning to the foreground reconnects at once with the attempt counter reset. The daemon's usual initial state after a connect is the resync.
- `src/backoff.ts`: 500 ms doubling, capped at 10 s, the native client's numbers.
- `src/hello.ts`: builds the `Hello`.
- `src/token-store.ts`: the `TokenStore` seam: `indexedDbTokenStore` keeps the pairing record `{host, port, token}` for the host the page was served from (database `rustling-tulip`, store `device`, key `token`), and `memoryTokenStore` is the tests' store.

## Build output

`dist/` holds `index.html`, `sw.js`, `manifest.webmanifest` and the icons at the top level under their stable names, and the hashed page assets under `assets/`. The daemon serves that folder from beside its install (`pwa`), or from the folder `RUSTLING_TULIP_PWA_DIR` names for a dev run: hashed assets are cached immutably, everything else is revalidated.

The folder reaches the daemon in three ways. For a dev run, `.\rt.ps1 build -Pwa` runs the install and build above and replaces `target\<profile>\pwa` with `dist/`. A release's `.\rt.ps1 installer` does the same and ships it as `pwa` in the install folder, which an update replaces by rename. On a Mac, `tools/installer/pack-macos.ps1` builds it and puts it in the app bundle's `Contents/Resources/pwa`, where a Mac daemon looks for it.

The icons are resized from `tools/installer/icon/icon-1024.png`; regenerate them when that artwork changes.
