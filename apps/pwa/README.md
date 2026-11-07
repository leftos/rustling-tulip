# rustling-tulip phone app

The web app a phone opens over the tailnet: the installable shell the daemon serves from its `pwa` folder (`crates/daemon/src/server/pwa_static.rs`), built with Vite and plain TypeScript. The service worker in `src/sw.ts` is hand-written; the runtime dependencies are `jsqr` (the QR scanner's fallback) and `@xterm/xterm` with `@xterm/addon-fit` (the terminal).

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

The page opens one WebSocket to `wss://{host}/ws` (its own origin) on load and sends `Hello` with its stored device token, `protocol_version` and `protocol_versions`, and no `client_id`. The connection status shows in the console (`pwa connection {kind: "status", state}`); an unpaired phone shows the pairing screen.

An unpaired phone gets `Welcome` then `DevicePairFailed { reason: "device not approved" }`, and the socket stays open as the pairing channel (state `pairing`); nothing redials by itself there. `Connection.askToPair()` sends the request (redialling first when the channel has closed), `Connection.redeem(code)` sends the six-digit code once the owner approves, and the `device_token` reply is stored and the page redials with it. A stored token the daemon refuses with `device not approved` is not presented again for the rest of that visit, and the phone falls back to pairing; the stored record is kept and tried again on the next launch, since the daemon says the same when it cannot read its own device store. Only `Connection.forget()` (which clears it) or a new pairing (which overwrites it) replaces the record. Any other refusal of a token stops at `notApproved`. `Connection.send(message)` sends any `ClientMessage` — a prompt answer, keys, text, a chat load — and answers `true` only while the connection is `ready`; in any other state it sends nothing and answers `false`, so the screen shows the connection as offline, and nothing is queued across a reconnect.

- `src/connection-machine.ts`: a pure reducer over the states idle, connecting, handshaking, ready, backoff, hidden, pairing and notApproved.
- `src/pairing.ts`: a pure reducer over the pairing steps (asking, pending, approved, denied, paired) and the error text the screen shows.
- `src/pairing-screen.ts`: what the pairing screen shows for a pairing model, connection status and camera state (pure); `src/pairing-view.ts` renders it and wires its buttons, typed code and scanner to the connection. The screen shows only on the pairing channel, or once paired until the connection is ready.
- `src/scanner.ts`: the `CodeScanner` seam: the rear camera read with `BarcodeDetector` when it reads QR codes, else jsQR on the main thread; it accepts only a six-digit code and releases the camera on stop.
- `src/keys.ts`: named keys and Ctrl chords to the bytes a terminal sends, the Keys tray in collie's order, and `keyInput`, which wraps bytes as a `SendInput`.
- `src/quick.ts`: collie's quick-reply groups (yes and no; continue, commit and push, retry, skip, drastically simplify; `y` and `n` for a plain shell), each sent as its text and Enter.
- `src/home-model.ts`: the Home list's model (pure): sessions grouped as the desktop sidebar groups them, rows that keep their place when their state changes, the Needs you tray (sessions waiting for an answer, longest wait first) and each card's one-tap answers.
- `src/home-actions.ts`: the Home's requests (pure): answer a card, Stop on a second tap, Resume as the desktop does (duplicating a stopped session and discarding the original, or resuming an abandoned one), and the replies that clear or refuse them.
- `src/chat-model.ts`: one session's chat (pure), from the chat mirror the desktop Chat view uses: `LoadChat` on entering, `UnloadChat` on leaving or hiding, a reload on every reconnect, and the desktop's display cap, unknown-kind filter and removed-id rule.
- `src/home-screen.ts`: what Home draws (pure): the Needs you tray's shown card, followed by session id, the groups and rows with Stop and Resume, a refused action's error and the Reconnecting… bar; `src/home-view.ts` renders it, repainting only when it changed, and owns Home's visibility: once shown it stays up through a reconnect and hides only for the pairing screen.
- `src/push-model.ts`: the Notifications panel's model (pure): what the panel shows (can't receive, add to Home Screen, blocked, or the `Allow notifications` switch and, with a subscription, `Needs you`, `Finished` and `Hold while I'm at my computer`), the switches disabled while a request or a browser step is in flight and until `push_status` is known, and the text under them (the daemon's refusal verbatim, a browser step's own failure, or `Offline`).

  Turning on asks the permission, subscribes with the daemon's VAPID key and sends `register_push` (the first enable turns every switch on), and a refused register after that subscribe drops the browser subscription; turning off sends `unregister_push`, then drops the subscription; a switch re-sends `register_push` and keeps its earlier value if refused.

- `src/push-port.ts`: the `PushPort` seam over permission, the service worker's push manager and the standalone check: `browserPushPort` is the real one, `memoryPushPort` the tests'. `src/base64url.ts` turns the VAPID key into the bytes `subscribe` takes.
- `src/notifications-view.ts`: renders the panel (thin) over Home as a full-screen sheet headed `Notifications` with `Done`, opened by the Home header's `Notifications` button; there is no route for it, and it hides whenever Home does.

  Each app open (connection ready and paired) sends `get_push_status`: when the phone and the daemon both hold an endpoint and they differ it re-registers with the daemon's switches, when the daemon holds none and the phone holds one it drops the phone's subscription, and when the phone holds none nothing changes (iOS can report none while pushes still arrive). That check never prompts or subscribes; only a tap does.

- `src/badge.ts`: the Home Screen icon badge: `createBadge` applies the Needs you count of each loaded Home model through the `BadgeEnv` seam (`browserBadgeEnv` is the real Badging API, null where the browser has none), clears it at 0, repeats nothing for an unchanged count, leaves a reconnect's empty model alone until the next `sessions` message, and logs a refused call (`setAppBadge failed`, `clearAppBadge failed`) so the next model with a different count applies again.
- `src/composer.ts`: the session composer (pure), following the desktop's send tracker: `ChatSend` with the draft kept until the sent text shows up as a pending user row, a 10 s backstop, a refusal (even a late one) shown verbatim with the text given back; Keys and Quick as `SendInput`.
- `src/prompt-sheet.ts`: the prompt sheet (pure): the prompt on screen as numbered buttons, each kind mapped to its `ChatAnswer` as the desktop's prompt cards do, Esc, ↑ and ↓ as keys, one answer at a time per prompt, and a swipe that hides it until the next prompt. The chat mirror writes the answered prompt's receipt row.
- `src/session-screen.ts`: what the session screen draws (pure) and `parseRoute` for `/s/{id}`; `src/session-view.ts` renders it: the chat, the composer with its Keys and Quick docks, and the prompt sheet, each part repainted only when it changed; the header's Terminal toggle swaps them for the terminal and its key bar (the Keys tray keys, Ctrl, Quick), and a session whose chat is not mirrored shows the terminal as its main view. Home's Open and a row tap push `/s/{id}`; Back and the browser's back return to Home.
- `src/terminal-model.ts`, `src/terminal-size.ts`, `src/terminal-keys.ts`: the phone terminal's pure parts: attaching a session by `LoadScrollback` with live output held until its own reply, the desktop's retries and resync rules; the shared PTY size claim (only typing or a pane resize claims); and the key bar with a sticky Ctrl.
- `src/terminal-pane.ts`, `src/terminal-view.ts`: the phone terminal drawn by xterm.js behind the `TerminalLike` seam: `createTerminalPane` feeds the attach model's output to the terminal and sends its replies as `automatic` input (`isTerminalReply` in `terminal-keys.ts`) and everything else as typing, which claims the PTY size; `mountTerminal` builds the real xterm and fit addon. xterm's CSS is imported at the top of `style.css`, which is why the page's CSP allows inline styles.
- `src/socket.ts`: the driver, with the socket, timers, visibility and token store injected. A Welcome that does not arrive in 10 s is a failure; backgrounding closes the socket and clears timers; returning to the foreground reconnects at once with the attempt counter reset. The daemon's usual initial state after a connect is the resync.
- `src/backoff.ts`: 500 ms doubling, capped at 10 s, the native client's numbers.
- `src/hello.ts`: builds the `Hello`.
- `src/token-store.ts`: the `TokenStore` seam: `indexedDbTokenStore` keeps the pairing record `{host, port, token}` for the host the page was served from (database `rustling-tulip`, store `device`, key `token`), and `memoryTokenStore` is the tests' store.

## Build output

`dist/` holds `index.html`, `sw.js`, `manifest.webmanifest` and the icons at the top level under their stable names, and the hashed page assets under `assets/`. The daemon serves that folder from beside its install (`pwa`), or from the folder `RUSTLING_TULIP_PWA_DIR` names for a dev run: hashed assets are cached immutably, everything else is revalidated.

The folder reaches the daemon in three ways. For a dev run, `.\rt.ps1 build -Pwa` runs the install and build above and replaces `target\<profile>\pwa` with `dist/`. A release's `.\rt.ps1 installer` does the same and ships it as `pwa` in the install folder, which an update replaces by rename. On a Mac, `tools/installer/pack-macos.ps1` builds it and puts it in the app bundle's `Contents/Resources/pwa`, where a Mac daemon looks for it.

The icons are resized from `tools/installer/icon/icon-1024.png`; regenerate them when that artwork changes.
