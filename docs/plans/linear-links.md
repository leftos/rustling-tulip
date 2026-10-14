# Linear issue links

Linear issue identifiers (`RT-116`) and Linear issue URLs are detected in terminal text and in session titles. Hovering one shows a card with the issue's title and status; clicking opens it in the Linear app, or on the web when the app is missing. Linear issue: RT-133.

Research: [linear-links.md](../research/linear-links.md). Code map at the time of design: links in `apps/native/src/links.rs` (`LinkKind { Url, Path }`), Ctrl-hover detection and Ctrl+click in `term_view.rs`, opening in `open_view.rs` / `open.rs`, labels through `display_label` (`sidebar.rs`), tooltips through `lib.rs` (`Tip`), popovers through `session_menu.rs` (`popover_frame`). Mock: [canvases/linear-card/linear-card.html](./canvases/linear-card/linear-card.html).

## Rulings

- **Card (owner):** the Standard card, mock section 1 lane B. It shows the identifier, title, state (Linear's state colour), priority, project, release, assignee and when it was last updated.
- **Session titles (owner):** an identifier in a session title is drawn as a state chip, mock section 2 lane B: a small chip holding the identifier and a dot in Linear's state colour. Hover shows the card, click opens the issue. This applies in the pane header and the sidebar leaf.
- **Credential (owner):** the daemon reads `LINEAR_API_KEY` through `user_env::resolve` (process environment, then `HKCU\Environment`), and through the env-secrets store once Wave 1b merges. A read-only personal API key scoped to the owner's teams is enough (`Authorization: {key}`, no `Bearer`).
- **Detection (owner):** an identifier counts only when its team key is one of the workspace's teams. The daemon fetches the keys (`teams { nodes { key } }`) and refreshes them daily. Any `linear.app/{workspace}/issue/{ID}` URL also counts. With no key configured, only URLs are detected.

- **Hover timing (owner):** in the terminal, a detected identifier is always underlined (mock [linear-card-states.html](./canvases/linear-card/linear-card-states.html) section 3, lane B). Resting the mouse on it for 500 ms shows the card; holding Ctrl over it shows the card at once (lane A). A session-title chip shows the card on plain hover, as its tooltip does today.
- **No data (owner):** the card keeps its shape with a one-line notice (mock section 4, lane A). With no key it says to set `LINEAR_API_KEY` and offers Open in Linear. While loading it says so. When Linear is unreachable it shows the cached copy with "Couldn't reach Linear · as of {age}" in the asking colour.
- **Refresh (owner):** the daemon fetches an issue the first time a title or a hover names it. It refreshes every issue a visible session title holds every 5 minutes, in one batched query. A hover on a copy older than 1 minute refetches it.
- **Branch (owner):** the work lands on `feat/linear-links` behind a draft PR, merged through `/ship` once the feature works.

- **Click (owner):** Ctrl+click on a terminal identifier opens it, as paths and URLs open today; a plain click keeps selecting text. RT-58 (plain-click links) later moves every link kind together. A click on a title chip opens the issue.
- **Phone (owner):** in the Android app, a tap on an identifier shows the same card from the daemon's cache, with an Open button. This lands in a phone wave, not in this one.
- **Placement (owner):** Wave 17, after Wave 16.

## Orchestrator decisions (not owner rulings; open to the owner)

- **Open target:** the client opens `linear://linear.app/{workspace}/issue/{ID}` (the help center's documented form) when `HKCU\Software\Classes\linear` (else `HKCR\linear`) is registered. Otherwise it opens the issue's `url` from the API, or `https://linear.app/{workspace}/issue/{ID}` before the issue is fetched. ShellExecute cannot detect a broken handler, so the registry check is the probe.
- **The daemon owns everything Linear:** the key, the team keys and workspace `urlKey`, the issue cache and its refresh, so remote and phone clients share one cache and one rate budget. It scans session titles itself, since it already holds every `terminal_title` and label. A client asks only for identifiers it sees in terminal text, on hover.
- **HTTP:** `reqwest` with rustls, the version `crates/daemon-client` already uses, added to the daemon; GraphQL at `https://api.linear.app/graphql` behind a transport trait, so tests use a fake. One aliased query fetches a batch of issues (at most 10,000 complexity points a query).
- **Protocol (additive, no bump):** `LinearContext` (daemon to every client, at connect and on change: team keys, workspace `urlKey`, whether a key is configured), `GetLinearIssues { identifiers, request_id }` (client to daemon), and `LinearIssues { issues }` (daemon to every client, since the cache is shared).

  `LinearIssue` holds the identifier, title, url, state (name, type, colour), priority and its label, project, releases, assignee and `updated_at`, plus `fetched_at` and a `stale` flag set when a refresh failed.
- **Testable core:** detection is a third pass in `links.rs` (`LinkKind::Issue`); the card's content and notice come from a plain-Rust model over the cache; the hover timer and the card view only render it.

## Steps

Order: LL.1 first; LL.2 and LL.4 after it, in parallel; LL.3 needs LL.2; LL.5 needs LL.4; LL.6 needs LL.3 and LL.5; LL.7 needs LL.6; LL.8 last. Each step lands on `feat/linear-links`.

- [ ] **LL.1 Protocol.** `LinearContext`, `GetLinearIssues`, `LinearIssues`, `LinearIssue` and its state type with `#[serde(other)] Unknown` where it holds an enum; doc comments. Proof, red first: round-trip tests and a `v22_compat` case that a v22 peer's messages still decode. Files: `crates/protocol/src/lib.rs`.
- [ ] **LL.2 Daemon Linear client.** New `crates/daemon/src/linear.rs`: the key from `user_env::resolve("LINEAR_API_KEY")`, the transport trait and its `reqwest` implementation, the team-keys and `urlKey` query (refreshed daily), the batched issue query, and the cache with `fetched_at` and staleness.

  Proof, red first: unit tests over a fake transport for parsing, batching, a missing key, an HTTP error marking entries stale, and identifier matching against team keys; `cargo deny check`. Files: `crates/daemon/src/linear.rs`, `crates/daemon/Cargo.toml`.
- [ ] **LL.3 Daemon wiring.** Send `LinearContext` after `Welcome`; scan titles and labels for identifiers and fetch new ones; refresh the identifiers in live session titles every 5 minutes in one query; answer `GetLinearIssues`, refetching copies older than 1 minute; broadcast `LinearIssues`. Proof: `test_hub` tests with the fake transport. Files: `crates/daemon/src/server.rs`, `linear.rs`.
- [ ] **LL.4 Native detection.** `LinkKind::Issue` in `links.rs`, matching only the context's team keys and `linear.app` issue URLs. Identifiers are always underlined in the terminal; Ctrl+click opens. Proof: `links.rs` unit tests (a known key matches; `UTF-8`, `SHA-256`, `ISO-8601` and unknown keys don't; a URL matches); a ui spec that Ctrl+click on an identifier asks the opener. Files: `apps/native/src/links.rs`, `term.rs`, `term_view.rs`, `open_view.rs`, `net.rs`.
- [ ] **LL.5 Native opener.** The `linear://` registry probe and the https fallback, as a validated `OpenJob`. Proof: `open.rs` tests over a fake registry and the fake `Opener`. Files: `apps/native/src/open.rs`.
- [ ] **LL.6 Native card.** The card model (fields, the three notices) over the client's copy of the cache. The hover timer shows it after 500 ms of rest, or at once while Ctrl is held. The card view uses `popover_frame`, anchored at the identifier's cells. Proof: model unit tests; ui specs for the timer, Ctrl, and each notice. Files: new `apps/native/src/linear_card.rs` and `linear_card_view.rs`, `term_view.rs`, `lib.rs`.
- [ ] **LL.7 Title chips.** An identifier in a session's display label becomes a chip (identifier plus a dot in the state colour) in the pane header and the sidebar leaf; hover shows the card, click opens. Proof: ui specs in `ui_sidebar` and the pane header's spec; `native-shot main` before and after for the owner. Files: `apps/native/src/sidebar.rs`, `sidebar_view.rs`, `grid_view.rs`.
- [ ] **LL.8 Docs.** `docs/architecture.md` (a Task Index row "Change Linear issue links"), `docs/native-client.md` (the card, the chips, the timing), CLAUDE.md (`LINEAR_API_KEY` in Environment variables), the README glossary; then this design's durable text is promoted and the file deleted.

The phone's card is a separate item in a phone wave, using the same `LinearIssues` messages.
