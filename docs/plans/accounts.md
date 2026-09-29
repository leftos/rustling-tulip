# Claude accounts: usage, switching and auto-switch

Built-in [claude-swap](https://github.com/leftos/claude-swap) (`X:\dev\claude-swap`, CLI `cswap`): the daemon holds several Claude Code logins, polls each one's 5-hour and weekly usage, switches the machine's active login by hand or on its own when one runs low, and lets every client (the native client, a remote native client, the phone) watch and switch. Step ids are CA.0, CA.1, …

## Rulings

- **Global swap.** One active account for the machine. A switch rewrites Claude Code's live credential under Claude Code's own lock pair; every running `claude` (interactive and headless) picks the new account up on its next message, with no restart. Sessions never get their own `CLAUDE_CONFIG_DIR`, so transcripts, the recover dialog, subagent streams and status tailing keep reading one Claude home.
- **Native Rust over cswap's store.** The daemon reimplements the logic in Rust and reads and writes cswap's on-disk store and locks, so `cswap` and the daemon run side by side and existing accounts carry over. No Python at runtime.
- **Auto-switch in the daemon**, off by default, ported from cswap's rules. It runs whether or not a client is connected.
- **Two engines never both switch.** The daemon honours cswap's locks and claim leases when polling. While its auto-switch is on it holds a lease file in the store; when it sees a live `cswap auto` (its state file written recently), it pauses its own engine and reports "cswap auto is running".
- **Adding an account, both ways:** capture the current login (as `cswap add`), and a daemon-driven login that runs `claude`'s login in a scratch config dir as a visible session, then captures it.
- **Account kinds:** OAuth, API key and setup token. API-key and setup-token accounts show no usage (the usage endpoint needs an OAuth token) but can be switched to.
- **Desktop UI:** a footer chip with the active account and its 5h/7d bars; clicking it opens a popover listing every account with usage, reset times and a Switch button, plus the auto-switch state. Add, remove, alias, re-login and the auto-switch rules live in a Settings → Accounts tab.
- **Remote rights:** a remote client (a remote native client over LAN or relay, or the phone) sees usage, switches the active account and changes auto-switch on/off and threshold. Adding, removing, aliasing and the daemon-driven login stay local. Credentials and tokens never cross the wire, to any client.
- **Notifications:** a desktop toast for an auto-switch and for "all accounts exhausted, earliest reset at …"; the same events become phone pushes with MA7, under their own toggle.
- **Platforms:** Windows (and Linux) first, with the credential writer behind a trait; the macOS Keychain writer is a line in [macos-compat.md](./macos-compat.md).
- **Direct API calls:** the accounts module calls Anthropic's OAuth account endpoints (usage, profile, token refresh) directly. This is an exception to CLAUDE.md's no-direct-API rule; no model call is made. The CLAUDE.md wording changes with CA.3.
- **Placement:** its own wave before the backlog in [MAIN.md](./MAIN.md); the phone's accounts screen is MA10 in [mobile-app.md](./mobile-app.md).

## How cswap works (the behaviour to port)

File and line references are to `X:\dev\claude-swap\src\claude_swap\`.

**Claude Code's files** (`paths.py:34-82`): the home is `$CLAUDE_CONFIG_DIR` else `~/.claude`; the live credential is `<home>/.credentials.json`; the global config is `<home>/.config.json` when that legacy file exists, else `($CLAUDE_CONFIG_DIR || $HOME)/.claude.json`. The daemon resolves the home through `transcripts::claude_home` so both agree.

**The store** (`paths.py:90-108`): `~/.claude-swap-backup` on Windows and macOS, `$XDG_DATA_HOME/claude-swap` on Linux.

- `sequence.json`: `{activeAccountNumber, lastUpdated, sequence:[n], accounts:{"n":{email, uuid, organizationUuid, organizationName, added, alias?, kind?:"api_key", disabled?}}}`.
- `configs/.claude-config-{n}-{email}.json`: a `~/.claude.json` snapshot; only `oauthAccount` is read back.
- `credentials/.creds-{n}-{email}.enc`: plain base64 (not encryption), plus `.enc.prev` and `.unclaimed-*.enc` stashes with `.unclaimed-manifest.json`. Windows Credential Manager is not used (it rejects entries over about 2,500 bytes).
- `settings.json` (threshold 90, interval 60 s, cooldown 300 s, hysteresis 10, strategy `best`, unhealthy ticks 3), `autoswitch_state.json`, `mappings.json`, `sessions/`, `cache/usage.json`, `cache/usage_history.json`, and `.lock` (an OS file lock: `msvcrt.locking` / `flock`).

**Credentials.** OAuth: `{"claudeAiOauth":{accessToken, refreshToken, expiresAt(ms), scopes, refreshTokenExpiresAt?}, mcpOAuth…}`. A setup token is wrapped as `{"claudeAiOauth":{accessToken, scopes:["user:inference"]}}`. An API key (`sk-ant-api…`) is activated by writing `primaryApiKey` into `~/.claude.json`, appending `key[-20:]` to `customApiKeyResponses.approved` and deleting `.credentials.json`; activating OAuth removes `primaryApiKey`.

**A switch** (`switcher.py:6702-7274`), all network I/O before any lock:

1. Optionally `GET /api/oauth/profile` to learn whose token the live credential is.
2. Take cswap's `.lock`, then Claude Code's credential locks `<home>/.oauth_refresh.lock` then `~/.claude.lock` (stale after 60 s), then `~/.claude.json.lock` (stale after 10 s). Claude Code's locks are `proper-lockfile` directories made with `mkdir`; the holder touches the mtime every 3 s; a waiter gives up after 9 s.
3. Back up the outgoing account (live credential into its `.enc`, live `~/.claude.json` into its config); an ownership classifier (`:7050`) stashes the bytes as unclaimed instead when they look like another account's, and handles Claude Code's in-place wipe of a rejected token (`:7105`).
4. Write the target credential to `.credentials.json`, merging in the live machine-shared `mcpOAuth*` and `pluginSecrets` keys (`credentials.py:199`).
5. Splice only `oauthAccount` into the live `~/.claude.json`.
6. Update `sequence.json`. A transaction rolls every step back in reverse on failure (`models.py:164`).

Every write is a temp file plus rename, retried on Windows sharing violations (winerror 5/32/33, 10 tries, 2 ms to 250 ms; `fsutil.py`). Claude Code drops its cached token when `.credentials.json`'s mtime changes, which is why running sessions follow the switch.

**Endpoints** (`oauth.py`):

- Usage: `GET https://api.anthropic.com/api/oauth/usage`, headers `Authorization: Bearer <accessToken>`, `anthropic-beta: oauth-2025-04-20`, 5 s timeout. Parsed: `five_hour{utilization, resets_at}`, `seven_day{…}`, `extra_usage{is_enabled, used_credits, monthly_limit, utilization, currency, resets_at}` (credits ÷ 100), `limits[]{scope.model.display_name, percent, resets_at}`. Headroom is `100 − max(pct)`.
- Profile: `GET https://api.anthropic.com/api/oauth/profile` → `account.uuid/email`, `organization.uuid`.
- Refresh: `POST https://platform.claude.com/v1/oauth/token` with `{grant_type:"refresh_token", refresh_token, client_id:"9d1c250a-e61b-44d9-88ed-5944d1962f5e"}`. A token counts as expired 5 minutes before `expiresAt`. Refresh tokens are one-time: spending a superseded one gives `invalid_grant` and kills that login for good. So the active account is refreshed only under Claude Code's lock pair, an inactive one from its backup, each behind a per-slot `.consume-{n}.lock`. Only a 400/401/403 carrying `error:"invalid_grant"` quarantines an account.

**Polling budget and cadence** (`poll_policy.py`, `usage_store.py`): about 28–30 requests per trailing hour per identity. Serve TTL and minimum interval 180 s, urgent 60 s, active at most 300 s, candidates 300–600 s, exhausted 600 s; a move of 1 point halves the interval, no move multiplies it by 1.5; 10 % jitter; after a 429 at least 360 s for an hour, backing off ×1.5 to 1800 s. Data up to 300 s old drives decisions. Failure backoff `30·2^(n−1)` capped at 600 s. Claim leases (`CLAIM_TTL_S=90`: lock → claim → fetch unlocked → record) stop two collectors fetching one account.

**Auto-switch** (`autoswitch.py`): the trigger is the binding window's utilization reaching the threshold (`proactive`), no headroom (`at-limit`), or unknown usage for 3 ticks (`failover`). A candidate must be under the threshold and beat the active account by the hysteresis; a 300 s cooldown applies except to at-limit and failover. When every account is over, rank by soonest recovery (hysteresis 300 s, horizon 4 h). The warm-up gate holds every decision until each non-quarantined account has been polled since start. Each tick polls the active account when due plus the stalest due candidate, and refetches all within 15 points of the threshold. Before activating a target, refresh it if it expires within 10 minutes. Pace (`pace.py`) is display only.

## Design

**Daemon module** `crates/daemon/src/accounts/`:

| File | Owns |
|---|---|
| `store.rs` | cswap's store: typed read and write of `sequence.json`, configs, `.enc` credentials, `settings.json`, `cache/usage.json`; the `.lock` file lock; temp-plus-rename with the Windows retry |
| `claude_locks.rs` | Claude Code's `proper-lockfile` directory locks, with the 3 s mtime touch, staleness and the 9 s wait |
| `oauth.rs` | usage, profile and refresh over `reqwest` (0.12, `rustls-tls`, already in `Cargo.lock` through `daemon-client`); base URLs injectable for tests; tokens never logged |
| `poll.rs` | the cadence policy, backoff and claim leases; parks when no client is connected and auto-switch is off |
| `switch.rs` | the switch transaction and rollback; a `CredentialWriter` trait with the file writer (the Keychain writer comes with macOS) |
| `auto.rs` | the auto-switch engine, its lease file and the cswap-auto detection |
| `login.rs` | add current login, add token, and the daemon-driven login |
| `mod.rs` | the `Accounts` handle the hub owns, and the snapshot it broadcasts |

**Connection origin.** LAN connections run through the same router as local ones (`server.rs`, `start_lan_listener` → `build_router`), so the daemon cannot tell them apart today. CA.1 passes an origin (`Local` / `Remote`) into `build_router` and on into `client_session`; the account-admin handlers refuse a `Remote` origin with `ActionFailed`, and `Welcome` tells the client its origin so it can disable those controls with a reason. The relay (MA2) hands its streams to the LAN acceptor, so they are `Remote` too.

**Protocol** (additive; `crates/protocol/src/lib.rs`, with the `add-protocol-message` skill):

- `AccountSnapshot`: number, email, alias, organization name, kind (`oauth` / `api_key` / `setup_token` with `#[serde(other)] Unknown`), active, disabled, usage status (cswap's `ok | token_expired | api_key | relogin_required | foreign_credential | no_credentials | unavailable`, with `Unknown`), 5h and 7d `{pct, resets_at}`, per-model limits, extra usage, fetched-at and error. No token, key or credential field, ever.
- `AutoSwitchState`: enabled, threshold, hysteresis, cooldown, `paused_for_cswap`, warm-up done, last decision.
- `ListAccounts` → `Accounts{accounts, auto}`; broadcast `AccountsUpdated` on every change.
- `SwitchAccount{number, request_id}`, `SetAutoSwitch{enabled, threshold_pct}` (any origin).
- `AddCurrentLogin`, `AddAccountToken{token}`, `RemoveAccount{number}`, `SetAccountAlias{number, alias}`, `StartAccountLogin`, `SetAutoSwitchRules{hysteresis, cooldown, strategy}` (local only). `AddAccountToken` is the one message that carries a secret, client to daemon, local only.
- `AccountEvent{kind: switched{from, to, trigger} | all_exhausted{earliest_reset_at} | quarantined{number} | unquarantined{number}}`, broadcast.

**Native client:** `footer.rs` gains the chip; new `accounts.rs` (model) and `accounts_view.rs` (popover); `settings_view.rs` gains an Accounts tab; `notify.rs` shows the toasts under a new Notifications toggle, "Account switches". A client whose `Welcome` says `Remote` shows the admin controls disabled, with "Only on the machine running the daemon".

## Steps

- [ ] **CA.0 Spike: store and CLI contract.** In `docs/spikes/claude-swap-store.md`: (a) the store's schema and whether it carries a version (read `migrations.py`), with a sample of every file from a real store, secrets redacted; (b) that Rust's `std::fs::File::lock` on `.lock` and cswap's `msvcrt.locking` exclude each other, measured with both running; (c) a real usage response and its rate-limit headers; (d) the `claude` CLI's non-interactive login command, and what `claude auth status --json` reports after it, in a scratch `CLAUDE_CONFIG_DIR`; (e) how a recent `cswap auto` shows itself (`autoswitch_state.json` write cadence). Rulings the later steps need go into this file's Design.
- [ ] **CA.1 Connection origin.** `server.rs` (`build_router`, `client_session`, `start_lan_listener`), `crates/protocol` (`Welcome.origin`, `#[serde(default)]`). Test: a LAN-origin connection is refused an admin message that a local one is allowed.
- [ ] **CA.2 Store.** `accounts/store.rs`, `accounts/claude_locks.rs`. Tests: round-trip cswap's own fixtures (`tests/fixtures/sample_config.json` and files from CA.0) byte-for-byte where cswap would write the same; a held `.lock` or Claude lock directory blocks and a stale one is taken; a sharing violation on rename is retried.
- [ ] **CA.3 OAuth client.** `accounts/oauth.rs`; the daemon's `Cargo.toml` gains `reqwest`; CLAUDE.md's no-direct-API sentence gains this exception. Tests against a local axum mock: usage parsing (including missing windows and extra usage), refresh with the consume lock, `invalid_grant` quarantines and a plain 401 does not, `Retry-After`.
- [ ] **CA.4 Usage poller.** `accounts/poll.rs`, `accounts/mod.rs`, hub wiring in `server.rs`; writes `cache/usage.json` so `cswap list` shows the same numbers. Tests with a fake clock: cadence moves with usage, 429 backoff, a claim lease held by another process skips the account, parking at zero clients.
- [ ] **CA.5 Protocol.** The messages above in `crates/protocol`, handlers in `server.rs`, `net.rs` decoding in the native client. `cargo test -p protocol v22_compat` stays green.
- [ ] **CA.6 Manual switch.** `accounts/switch.rs`: the transaction in cswap's lock order, the ownership classifier, the shared-key merge, the `oauthAccount` splice, rollback, and the API-key path. Tests in a temp home: the outgoing account is backed up, a foreign credential is stashed, a failure mid-way leaves the live files as they were. An `#[ignore]`d contract test runs `cswap list --json` against the daemon-written store when `cswap` is on `PATH`.
- [ ] **CA.7 Footer chip and popover.** `footer.rs`, new `accounts.rs` and `accounts_view.rs`. UI spec `tests/ui_accounts.rs`: the chip shows the active account and bars; Switch sends `SwitchAccount`; a `relogin_required` row shows its reason; a stale fetch shows its age.
- [ ] **CA.8 Settings → Accounts.** `settings_view.rs`: list with alias, remove, "Add current login" (with the warning not to `/logout` first, since that can revoke the account being left), "Add token", auto-switch on/off, threshold, hysteresis, cooldown; admin controls disabled with a reason on a `Remote` origin. UI spec in `ui_accounts.rs`.
- [ ] **CA.9 Auto-switch engine.** `accounts/auto.rs`: the rules above, the warm-up gate, freshen-before-activate, the lease file, pausing while `cswap auto` runs, `AccountEvent`s. Tests with a fake clock and scripted usage: proactive, at-limit and failover triggers; hysteresis and cooldown hold; all-exhausted picks soonest recovery and emits its event; a recent cswap state file pauses the engine.
- [ ] **CA.10 Toasts.** `notify.rs`, `settings_view.rs` (the "Account switches" toggle). UI spec: an `AccountEvent` raises a toast only when the toggle is on.
- [ ] **CA.11 Daemon-driven login.** `accounts/login.rs`, `server.rs`, `settings_view.rs` ("Log in a new account…"): spawns the login found in CA.0 in a scratch `CLAUDE_CONFIG_DIR` as a visible plain session; when it exits logged in, captures it as an account and removes the scratch dir. Local only. Live e2e: `fake-claude` gains a login mode that writes a credential and `.claude.json`.
- [ ] **CA.12 Docs.** `docs/architecture.md` (the component and a task-index row), CLAUDE.md ("Where things live on disk": the store lives outside our config dir, and is cswap's), the README glossary checked against the shipped terms; this subplan's durable text promoted and the file deleted.

Phone and remote work outside this wave: MA10 in [mobile-app.md](./mobile-app.md) is the phone's accounts screen over these messages; MA7's pushes carry `AccountEvent`s; a remote native client (Phase 6) gets the chip and popover with no extra work, and the admin controls disabled.

## Pitfalls to keep

- Never refresh the active account's token outside Claude Code's lock pair, and never spend one refresh token twice: both kill the login.
- No network call while holding any lock.
- A token, key or credential byte never reaches a log line, a protocol message (other than the local `AddAccountToken`) or `state.json`.
- `CLAUDE_SECURESTORAGE_CONFIG_DIR` set means cswap refuses to refresh (`store-unmirrored`); the daemon does the same and shows the reason.
- Codex and Cursor sessions, and Claude sessions on another provider (DeepSeek, whose routing variables set their own credential), are untouched by a switch. The chip says which account Anthropic sessions use, not every session.
- An `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` in the daemon's own environment overrides the credential file for every session it spawns; the accounts module detects that and shows it as the reason switching has no effect.
