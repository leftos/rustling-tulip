# Claude accounts: usage, switching and auto-switch

The daemon holds several Claude Code logins, polls each one's 5-hour and weekly usage, switches the machine's active login by hand or on its own when one runs low, and lets every client (the native client, a remote native client, the phone) watch and switch.

It does this through the `claude-accounts` crate, vendored into this repo at `crates/agents/claude/accounts/` and owned here: the crate owns the account store, the locks, the refresh and switch transactions, the poll cadence, the pace maths and the auto-switch engine; the daemon supplies the HTTP transports, a process table, the drivers and the protocol. Nothing here depends on another project.

The accounts feature lands on the feature branch `feat/claude-accounts`; RT-39 is its parent and RT-908 to RT-916 and RT-919 are its parts. Step ids are CA.0, CA.1, …

## Rulings

- **Global swap.** One active account for the machine. A switch rewrites Claude Code's live credential under Claude Code's own lock pair; every running `claude` (interactive and headless) picks the new account up on its next message, with no restart. Sessions never get their own `CLAUDE_CONFIG_DIR`, so transcripts, the recover dialog, subagent streams and status tailing keep reading one Claude home.
- **Own store, native Rust.** The account store lives in rustling-tulip's data folder (`%LOCALAPPDATA%\leftos\rustling-tulip\data\accounts\`, following `docs/on-disk.md`'s data-dir rules) and starts empty. The user adds logins in Settings → Accounts. There is no import of any other store and no notice about one. Its seal format identifiers are its own (RT-927). No Python at runtime.
- **Auto-switch in the daemon**, off by default, using the crate's rules. It runs whether or not a client is connected.
- **Two engines never both switch.** While its auto-switch is on, the daemon's engine holds a lease file in the store, so two engines on rustling-tulip's store (two daemons, or a daemon and its own CLI) cannot both switch; the one that does not hold the lease reports `Paused` with the holder as its reason.
- **Adding an account, both ways:** capture the current login, and a daemon-driven login that runs `claude`'s login in a scratch config dir as a visible session, then captures it.
- **Account kinds:** OAuth only. API-key and setup-token accounts were cut (RT-66); the crate still reads an `api_key` record but neither polls nor switches to it.
- **Desktop UI:** a footer chip with the active account and its 5h/7d bars; clicking it opens a popover listing every account with usage, reset times and a Switch button, plus the auto-switch state. Add, remove, alias, re-login and the auto-switch rules live in a Settings → Accounts tab.
- **Remote rights:** a remote client (a remote native client over the tailnet, or the iPhone PWA) sees usage, switches the active account and changes auto-switch on/off and threshold. Adding, removing, aliasing and the daemon-driven login stay local. Credentials and tokens never cross the wire, to any client. The tailnet is the only remote path.
- **Notifications:** a desktop toast for an auto-switch and for "all accounts exhausted, earliest reset at …"; the same events become phone pushes with MA7, under their own toggle.
- **Platforms:** Windows and macOS. The crate gains Unix locks and reads and writes the macOS live credential through the Keychain (RT-916); Accounts shows on macOS once that lands. Linux is unsupported and Accounts is hidden there. The credential writer sits behind a trait.
- **Direct API calls:** the accounts module calls Anthropic's OAuth account endpoints (usage, profile, token refresh) directly. This is an exception to CLAUDE.md's no-direct-API rule; no model call is made. The CLAUDE.md wording changes with CA.3.
- **Placement:** its own wave before the backlog in [MAIN.md](./MAIN.md); the phone's accounts screen is MA10 in [mobile-app.md](./mobile-app.md).

## Decisions on the crate

- **The API.** The own-store API: `AccountsRoot`, `Store`, `capture_live_login`, `switch`, `Engine`. The crate is a workspace member at `crates/agents/claude/accounts/` (package `claude-accounts`, `publish = false`, edition 2024), reached only through the Claude plugin's seam, since app code never names an agent (RT-908). Its README records the crate's behaviour: Switch, Engine, Pace and where the account store lives.

  It brings `proc-table` (git, `leftos/gate`) and `jiff`, and raises `zeroize` and `serde_json` (features `preserve_order`, `float_roundtrip`) for the whole workspace, so `Cargo.lock` is checked with `cargo tree -d`.
- **Remove, alias and settings.** The crate gains `Store::remove_account`, `Store::set_alias` and a writer for the `autoswitch` settings (RT-919). Account settings never go in the daemon's `state.json`.
- **Engine lease.** The daemon's engine stays off by default. The lease keeps two engines on rustling-tulip's store from both switching.
- **Transports** are synchronous: reqwest 0.12's `blocking` feature in the Claude plugin, each client built per call inside the blocking thread (the poller reuses clients when it lands). The daemon's async client is unchanged.
- **`ProcessTable`** is `proc-table`'s Windows table, and a macOS table from RT-916; on Linux it is the empty table.
- **Paid fallback and prepaid balances** are not surfaced: `maySpendCredits` stays off and `read_balance` keeps its default.
- **Daemon-driven login** points `ClaudePaths` at the scratch dir and calls `capture_live_login`, extended in the crate if it cannot yet take a scratch config dir.

## What the daemon still owns

**Claude's home.** The home is `$CLAUDE_CONFIG_DIR` else `~/.claude`; the daemon resolves it through `claude_home` in `crates/agents/claude/src/transcripts.rs`, which feeds `ClaudePaths::new(config_home, global_config)`, so the daemon and Claude Code agree.

**Pitfalls the crate enforces and RT must not bypass.** Refresh tokens are one-time, so a transport never retries a refresh POST, and the active account is refreshed only under Claude Code's lock pair; no network call under a lock; every HTTP status returns `Ok` from a transport, never `error_for_status`. `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` detection is RT's, not the crate's.

## Design

**Daemon module** `crates/daemon/src/accounts/`:

| File | Owns |
|---|---|
| transports (plugin-side, `crates/agents/claude/src/accounts.rs`) | the reqwest `blocking` implementations of the crate's `UsageTransport`, `RefreshTransport` and `ProfileTransport` (`UsageClient`, `RefreshClient`, `ProfileClient`): HTTPS only, no proxy, no redirects, bodies capped at 64 KiB, a sensitive `Authorization`, a zeroizing refresh body; one blocking HTTP client built lazily on a blocking thread and dropped on its own thread; `Retry-After` read as seconds or an HTTP date; the base URL injectable for tests through `AccountsConfig::base_url`, the only case plain `http` is accepted; tokens never logged. The daemon never depends on the crate. |
| process table (plugin-side, `crates/agents/claude/src/accounts.rs`) | the `ProcessTable` for `TickInputs.processes` and the lease (`process_table()`): `proc-table`'s Windows table, the macOS table, the empty table on Linux |
| `driver.rs` | the poll and engine tasks in `spawn_blocking`, the lease, the `Drain`, a settings snapshot; parks the poller when no client is connected and no engine is live; serves the refresh lane a write uses to re-read the store |
| `mod.rs` | the handle: the cached wire snapshot `ListAccounts` answers from, the refresh lane's sender, the driver's start and bounded stop |
| `snapshot.rs` | the plugin's `agent_api` rows (`AccountsSnapshot` and its row types, built by `ClaudeAccounts`) to protocol rows; the daemon never names the crate's types |
| `login.rs` | the daemon-driven login and the add-current-login handler |
| `mod.rs` | the `Accounts` handle the hub owns, and the snapshot it broadcasts |

**Connection origin.** `client_session` already knows each connection's `AuthKind` (`Local` on the loopback listener, `Tailnet` on the tailnet listener), in `crates/daemon/src/server/connection.rs`. CA.1 maps it to an origin (`Local` / `Remote`); the SSH bridge counts as `Local`, so there are two origins. The account-admin handlers refuse a `Remote` connection through `host_only_refusal`, which answers `Error{code: local_only}` (RT-911 extends it), and `Welcome` tells the client its origin so it can disable those controls with a reason.

**Protocol** (additive; `crates/protocol/src/lib.rs`, with the `add-protocol-message` skill):

- `AccountSnapshot`: number, email, alias, organization name, kind (`oauth` / `api_key`, with `#[serde(other)] Unknown`), active, disabled, usage status (from the crate's `PollResult` and `decision_value`: `ok`, `token_expired`, `api_key`, `relogin_required`, `foreign_credential`, `no_credentials`, `store_unwritable`, `unavailable`, `Unknown`), 5h and 7d `{pct, resets_at}`, per-model limits, extra usage (`enabled`, `user_disabled`, `spend_limit_reached`, nullable `limit`), fetched-at, error.

  No token, key or credential field, ever.
- `AutoSwitchState`: enabled, threshold, hysteresis, cooldown, `paused_reason: Option<String>` (the lease holder, or the store's refusal), warm-up done, last decision, and an optional fleet verdict (the crate's `WindowVerdict`, with the unit of its `rate_window`).
- `ListAccounts` → `Accounts{accounts, auto}`; broadcast `AccountsUpdated` when a poll or a write's refresh changes the snapshot. A write is answered with `Accounts{.., request_id}` or `Error{code: account_*}`.
- `SwitchAccount{number, request_id}`, `SetAutoSwitch{enabled, threshold_pct}` (any origin).
- `AddCurrentLogin`, `RemoveAccount{number}`, `SetAccountAlias{number, alias}`, `SetAccountOrder{order}`, `StartAccountLogin`, `SetAutoSwitchRules{hysteresis, cooldown, strategy}` (local only). No message carries a secret.
- `AccountEvent{kind: switched{from, to, trigger} | all_exhausted{earliest_reset_at} | quarantined{number} | unquarantined{number}}`, and `halted{message}`, broadcast (in the protocol since RT-967, listed as `account_events` in `Welcome.features`; today only `switched` with trigger `manual` is sent, after a `SwitchAccount` that moved the login, and the engine's events come with RT-968). The fleet verdict ("All accounts blocked {when} until {when}") rides the attention feed (RT-87).

**Native client:** `footer.rs` gains the chip; new `accounts.rs` (model) and `accounts_view.rs` (popover); `settings_view.rs` gains an Accounts tab, hidden on Linux; `notify.rs` shows the toasts under a new Notifications toggle, "Account switches". A client whose `Welcome` says `Remote` shows the admin controls disabled, with "Only on the machine running the daemon".

## Steps

Sub-issues: RT-908 vendored the crate (done: CA.0, and the vendoring half of CA.2); RT-919 the store writers, the accounts dir and the transports (the rest of CA.2, CA.3); RT-909 the origin and the read-only protocol (CA.1, CA.5 read half); RT-910 the poller (CA.4); RT-911 admin and switch (CA.5 write half, CA.6); RT-912 auto-switch (CA.9); RT-913 the chip (CA.7); RT-914 the Settings page and toasts (CA.8, CA.10); RT-915 login and docs (CA.11, CA.12); RT-916 macOS (Unix locks and the Keychain); RT-927 the fork's own seal format identifiers.

- [ ] **CA.0 Spike: the crate's contract** (RT-908). In `docs/spikes/accounts-crate.md`: (c) a real usage response and its rate-limit headers, to check the transport keeps `Retry-After`. (d) The `claude` CLI's non-interactive login command, and what `claude auth status --json` reports after it, in a scratch `CLAUDE_CONFIG_DIR`.

  Also: whether `capture_live_login` on a scratch `ClaudePaths` takes the scratch login without touching the live one.

  Rulings the later steps need go into this file's Design.
- [ ] **CA.1 Connection origin** (RT-909). `crates/daemon/src/server/connection.rs` (`AuthKind`, `client_session`), `crates/protocol` (`Welcome.origin`, `#[serde(default)]`). Test: a tailnet connection is refused an admin message that a local one is allowed.
- [ ] **CA.2 The crate** (RT-908, RT-919, RT-927). The crate at `crates/agents/claude/accounts/` as a workspace member, the lockfile resolved (`jiff`, `proc-table`, `zeroize`, `serde_json` features). The store's accounts dir under the data folder, the writers for remove, alias and settings, and the crate's own seal format identifiers. A smoke test that `Store::open(AccountsRoot::from_local_app_data(..))` and `read_sequence` work on a temp root.

  `Store::write_refusal()` is checked before every store write and refresh spend; `StoreErrorKind` is `#[non_exhaustive]`, so a wildcard arm, with messages for `AccountReidentified`, `StoreReadOnly`, `UnknownSchema` and `RootUnsafe`. The lock folder exists before `FileLock::acquire`. No fixtures of this repo's own: the crate's tests pin the format.
- [x] **CA.3 Transports** (RT-919). The transports are in `crates/agents/claude/src/accounts.rs`, whose `Cargo.toml` gains reqwest's `blocking` feature, and the plugin reaches them through the optional `Agent::accounts` seam (not called yet); CLAUDE.md's no-direct-API sentence gains this exception (still to do). `get_prepaid` stays `Unsupported`. Tests (`crates/agents/claude/tests/accounts.rs`) against a std `TcpListener` server: a 429 keeps `Retry-After` through `RawUsage`, and a 400 `invalid_grant` body reaches the crate's classifier. Parsing, refresh, the consume lock and quarantine are the crate's.
- [x] **CA.4 Usage poller** (RT-910). `accounts/driver.rs`, `accounts/snapshot.rs`, `accounts/mod.rs`, hub wiring in `server/`.

  A `spawn_blocking` task calls `poll_with_refresh(&transport, &ctx, FetchScope::..)` on the poll's own `next_poll_in` (the earliest saved `next_poll_at` of enabled accounts, never under 60 s; 60 s after an error; the crate's `next_delay` stays the engine's, RT-912); the crate persists the cadence and the shared `cache/usage.json`, so every process on the store spends one budget.

  The `RefreshContext` is built once (store, `ClaudePaths` from `claude_home`, transports, `Drain`, `ProbeVerdicts`, `LockTiming`, clock). Parking at zero clients stays this repo's rule, except while an engine is live. `UsageTable` becomes `AccountSnapshot`, then `AccountsUpdated`.

  Shutdown (every path that flips the shutdown watch) calls `begin_shutdown` for up to 15 s, during an in-flight poll if one runs, and `run()` waits up to 20 s for the driver. `ListAccounts` answers from the cache and wakes a no-account park; a client connecting restarts polling after one; `AccountsUpdated` goes out only when the mapped snapshot changes. Tests with a fake clock around the driver, not the cadence maths.
- [ ] **CA.5 Protocol** (RT-909 read half: `Welcome.origin`, `ListAccounts`, `Accounts`, `AccountsUpdated`; RT-911 write half: the account write messages; `SetAutoSwitch` and `AccountEvent` are RT-967; the engine's events come with RT-968). The messages above in `crates/protocol`, handlers in `server/`, `net.rs` decoding in the native client. `cargo nextest run -p protocol --cargo-profile test-release` stays green.
- [ ] **CA.6 Manual switch** (RT-911). The handler calls `probe_outgoing`, then `switch(&ctx, number, &probe, precondition)` in `spawn_blocking` (a manual switch passes no precondition), and maps `SwitchOutcome` to the refreshed `Accounts` answer (and broadcasts `AccountEvent::switched` with trigger `manual`) and `SwitchErrorKind` to `Error { code: account_* }`, `account_draining` on shutdown. The add-current-login handler calls `capture_live_login` (`CaptureKind::{Added, Refreshed, Kept}`), local origin only.

  Remove and alias call `Store::remove_account` and `Store::set_alias`; remove refuses the active account and the live login; the origin gate refuses `Remote`. Tests are the handler mapping and the origin gate: the transaction, rollback and ownership classification are the crate's.
- [x] **CA.7 Footer chip and popover** (RT-913). `footer.rs`, new `accounts.rs` and `accounts_view.rs`. UI spec `tests/ui/ui_accounts.rs`: the chip shows the active account and bars; Switch sends `SwitchAccount`; a `relogin_required` row shows its reason; a stale fetch shows its age; a "store read-only" or "held by another engine" row shows its reason. No paid-fallback state.
- [ ] **CA.8 Settings → Accounts** (RT-914). `settings_view.rs`: list with alias, remove, "Add current login" (with the warning not to `/logout` first, since that can revoke the account being left), auto-switch on/off, threshold, hysteresis, cooldown, strategy; the tab is hidden on Linux; admin controls disabled with a reason on a `Remote` origin. Auto-switch settings go through the crate's settings writer. UI spec in `ui_accounts.rs`.
- [ ] **CA.9 Auto-switch engine** (RT-912). `accounts/driver.rs`: a loop around `Engine::new(owner, pid, warm_since)` and `Engine::tick(&TickInputs{context, collector: &TransportCollector(&usage), processes, switcher: &LiveSwitch, claude_config_dir_set, settings, auto_switch_enabled})`, sleeping on `next_delay` and `sleep_event`. The engine claims and heartbeats the `Lease`; the driver releases it on shutdown and on `halt`.

  `Event`s map to `AccountEvent` and toasts. The engine is off by default. `TickOutcome::Paused{reason}` surfaces as `paused_reason`; `TickOutcome::Halted` (a failed rollback) is a user-visible error. The rules, warm-up, freshen, trigger, ranking, hysteresis, cooldown and quarantine are the crate's. Tests: the driver (a settings change wakes the loop, a `Paused` reason surfaces, drain stops it), not the rules.
- [ ] **CA.10 Toasts** (RT-914). `notify.rs`, `settings_view.rs` (the "Account switches" toggle). UI spec: an `AccountEvent` raises a toast only when the toggle is on.
- [ ] **CA.11 Daemon-driven login** (RT-915). `accounts/login.rs`, `server/`, `settings_view.rs` ("Log in a new account…"): spawns the login found in CA.0 in a scratch `CLAUDE_CONFIG_DIR` as a visible plain session; when it exits logged in, captures it with `capture_live_login` on a `ClaudePaths` for that dir and removes the scratch dir. Local only. Live e2e: `fake-claude` gains a login mode that writes a credential and `.claude.json`.
- [ ] **CA.12 Docs** (RT-915). `docs/architecture.md` (the component and a task-index row), `docs/on-disk.md` (the store under the data folder, outside the config dir), the `docs/README.md` glossary checked against the shipped terms; this subplan's durable text promoted and the file deleted.

Phone and remote work outside this wave: MA10 in [mobile-app.md](./mobile-app.md) is the phone's accounts screen over these messages; MA7's pushes carry `AccountEvent`s; a remote native client (Phase 6) gets the chip and popover with no extra work, and the admin controls disabled. Accounts on macOS is RT-916; Linux is unsupported; the fleet verdict in the attention feed is RT-87.

## Pitfalls to keep

- Never refresh the active account's token outside Claude Code's lock pair, and never spend one refresh token twice: both kill the login. The crate does both; do not bypass it.
- No network call while holding any lock. The crate enforces it; the transports never hold one.
- A token, key or credential byte never reaches a log line, a protocol message or `state.json`.
- `CLAUDE_SECURESTORAGE_CONFIG_DIR` set means the crate refuses to refresh (`RefreshContext.securestorage_dir_set`); the daemon passes it and shows the reason.
- Codex and Cursor sessions, and Claude sessions on another provider (DeepSeek, whose routing variables set their own credential), are untouched by a switch. The chip says which account Anthropic sessions use, not every session.
- An `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` in the daemon's own environment overrides the credential file for every session it spawns; the accounts module detects that and shows it as the reason switching has no effect.
