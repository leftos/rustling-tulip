# Claude accounts: usage, switching and auto-switch

The daemon holds several Claude Code logins, polls each one's 5-hour and weekly usage, switches the machine's active login by hand or on its own when one runs low, and lets every client (the native client, a remote native client, the phone) watch and switch.

It does this through the `claude-accounts` crate, forked from ops-tower into this repo at `crates/agents/claude/accounts/` and owned here: the crate owns the account store, the locks, the refresh and switch transactions, the poll cadence, the pace maths and the auto-switch engine; the daemon supplies the HTTP transports, a process table, the drivers and the protocol. ops-tower is maintenance-only and obsolete, so nothing here waits on it or asks it for changes.

The fork starts from ops-tower `feat/own-account-store` at `59dc25f` (the crate's own account store) with `d80c605`'s crate change (the 7-day-only fleet verdict) ported onto it (RT-908). The accounts feature lands on the feature branch `feat/claude-accounts`; RT-39 is its parent and RT-908 to RT-916 are its parts. Step ids are CA.0, CA.1, …

## Rulings

- **Global swap.** One active account for the machine. A switch rewrites Claude Code's live credential under Claude Code's own lock pair; every running `claude` (interactive and headless) picks the new account up on its next message, with no restart. Sessions never get their own `CLAUDE_CONFIG_DIR`, so transcripts, the recover dialog, subagent streams and status tailing keep reading one Claude home.
- **Native Rust over cswap's store.** The daemon takes ops-tower's `claude-accounts` as a git dependency (RT-66) and reads and writes cswap's on-disk store and locks, so `cswap` and the daemon run side by side and existing accounts carry over. No Python at runtime.

  Superseded: the crate's store is ops-tower's own, `%LOCALAPPDATA%\ops-tower\accounts` (`accounts.json`, sealed `credentials/<n>.cred`), never cswap's; the daemon and ops-tower share it through the crate's locks and cswap is not read (decision: ops-tower's own account store, RT-66). Accounts reach it through ops-tower's `import-cswap` or a login.
- **Auto-switch in the daemon**, off by default, using the crate's rules. It runs whether or not a client is connected.
- **Two engines never both switch.** The daemon honours cswap's locks and claim leases when polling. While its auto-switch is on it holds a lease file in the store; a live `cswap auto` is found by the crate's process scan (`runs_cswap_auto`, over the command line), since `autoswitch_state.json` is written only on a switch or a quarantine and says nothing about a running engine; then it pauses its own engine and reports "cswap auto is running".

  Superseded: cswap's engine cannot touch the store, and the cswap scan, `runs_cswap_auto` and "cswap auto is running" are gone from the crate. The crate's engine lease is the only exclusion: when ops-tower's engine and the daemon's both run, the lease picks one, and the other reports `Paused` with the holder as its reason (decision: engines and the lease, RT-66). The intent, never two switching engines, stands.
- **Adding an account, both ways:** capture the current login (as `cswap add`), and a daemon-driven login that runs `claude`'s login in a scratch config dir as a visible session, then captures it.
- **Account kinds:** OAuth only. API-key and setup-token accounts were cut (OPS-157, RT-66); the crate still reads an `api_key` record but neither polls nor switches to it.
- **Desktop UI:** a footer chip with the active account and its 5h/7d bars; clicking it opens a popover listing every account with usage, reset times and a Switch button, plus the auto-switch state. Add, remove, alias, re-login and the auto-switch rules live in a Settings → Accounts tab.
- **Remote rights:** a remote client (a remote native client over the tailnet, or the iPhone PWA) sees usage, switches the active account and changes auto-switch on/off and threshold. Adding, removing, aliasing and the daemon-driven login stay local. Credentials and tokens never cross the wire, to any client. The tailnet is the only remote path.
- **Notifications:** a desktop toast for an auto-switch and for "all accounts exhausted, earliest reset at …"; the same events become phone pushes with MA7, under their own toggle.
- **Platforms:** Windows (and Linux) first, with the credential writer behind a trait; the macOS Keychain writer is a line in [macos-compat.md](./macos-compat.md).

  Superseded: Windows and macOS. The forked crate gains Unix locks and reads and writes the macOS live credential through the Keychain (RT-916); Accounts shows on macOS once that lands. Linux stays unsupported and Accounts is hidden there (owner ruling on RT-39: fork the crate and own it, macOS support included).
- **Direct API calls:** the accounts module calls Anthropic's OAuth account endpoints (usage, profile, token refresh) directly. This is an exception to CLAUDE.md's no-direct-API rule; no model call is made. The CLAUDE.md wording changes with CA.3.
- **Placement:** its own wave before the backlog in [MAIN.md](./MAIN.md); the phone's accounts screen is MA10 in [mobile-app.md](./mobile-app.md).

## Decisions on the crate

- **The fork and its API.** The own-store API: `AccountsRoot`, `Store`, `capture_live_login`, `switch`, `Engine`. The crate is a workspace member at `crates/agents/claude/accounts/` (package `claude-accounts`, `publish = false`, edition 2024), reached only through the Claude plugin's seam, since app code never names an agent. Its README records the ops-tower shas it was forked from.

  It brings `proc-table` (git, `leftos/gate`) and `jiff`, and raises `zeroize` and `serde_json` (features `preserve_order`, `float_roundtrip`) for the whole workspace, so `Cargo.lock` is checked with `cargo tree -d`.
- **Remove, alias and settings.** The ops-tower crate has no remove, alias or settings writer, so the fork gains `Store::remove_account`, `Store::set_alias` and a writer for the `autoswitch` settings (RT-908). Account settings never go in the daemon's `state.json`.
- **Engine lease.** The daemon's engine stays off by default. A machine still running ops-tower shares the store through the lease, which picks one engine.
- **Transports** are synchronous: reqwest 0.12's `blocking` feature on one client built once, called inside `spawn_blocking`. The daemon's async client is unchanged.
- **`ProcessTable`** is `proc-table`'s Windows table, and a macOS table from RT-916; on Linux it is the empty table.
- **Paid fallback and prepaid balances** are not surfaced: `maySpendCredits` stays off and `read_balance` keeps its default. The engine still honours a paid fallback ops-tower's engine entered.
- **Daemon-driven login** points `ClaudePaths` at the scratch dir and calls `capture_live_login`, extended in the fork if it cannot yet take a scratch config dir.

## What the daemon still owns

**Claude's home.** The home is `$CLAUDE_CONFIG_DIR` else `~/.claude`; the daemon resolves it through `claude_home` in `crates/agents/claude/src/transcripts.rs`, which feeds `ClaudePaths::new(config_home, global_config)`, so the daemon and Claude Code agree.

**Pitfalls the crate enforces and RT must not bypass.** Refresh tokens are one-time, so a transport never retries a refresh POST, and the active account is refreshed only under Claude Code's lock pair; no network call under a lock; every HTTP status returns `Ok` from a transport, never `error_for_status`. `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` detection is RT's, not the crate's.

The crate's behaviour was documented in ops-tower's `docs/accounts.md` (Switch, Engine, Pace, Where the account store lives); RT-908 brings the parts the fork keeps into the crate's README.

## Design

**Daemon module** `crates/daemon/src/accounts/`:

| File | Owns |
|---|---|
| `transport.rs` | the reqwest `blocking` implementations of the crate's `UsageTransport`, `RefreshTransport` and `ProfileTransport`: HTTPS only, no proxy, no redirects, bodies capped at 64 KiB, a sensitive `Authorization`, a zeroizing refresh body; base URLs injectable for tests; tokens never logged |
| `process_table.rs` | the `ProcessTable` for `TickInputs.processes` and the lease: `proc-table`'s Windows table, the empty table elsewhere |
| `driver.rs` | the poll and engine tasks in `spawn_blocking`, the lease, the `Drain`, a settings snapshot; parks the poller when no client is connected and no engine is live |
| `snapshot.rs` | the crate's `UsageEntry`, `Sequence` and `AutoswitchState` to protocol rows |
| `login.rs` | the daemon-driven login and the add-current-login handler |
| `mod.rs` | the `Accounts` handle the hub owns, and the snapshot it broadcasts |

**Connection origin.** `client_session` already knows each connection's `AuthKind` (`Local` on the loopback listener, `Tailnet` on the tailnet listener), in `crates/daemon/src/server/connection.rs`. CA.1 maps it to an origin (`Local` / `Remote`); the account-admin handlers refuse a `Remote` origin with `ActionFailed`, and `Welcome` tells the client its origin so it can disable those controls with a reason.

**Protocol** (additive; `crates/protocol/src/lib.rs`, with the `add-protocol-message` skill):

- `AccountSnapshot`: number, email, alias, organization name, kind (`oauth` / `api_key`, with `#[serde(other)] Unknown`), active, disabled, usage status (from the crate's `PollResult` and `decision_value`: `ok`, `token_expired`, `api_key`, `relogin_required`, `foreign_credential`, `no_credentials`, `store_unwritable`, `unavailable`, `Unknown`), 5h and 7d `{pct, resets_at}`, per-model limits, extra usage (`enabled`, `user_disabled`, `spend_limit_reached`, nullable `limit`), fetched-at, error.

  No token, key or credential field, ever.
- `AutoSwitchState`: enabled, threshold, hysteresis, cooldown, `paused_reason: Option<String>` (the lease holder, or the store's refusal), warm-up done, last decision, and an optional fleet verdict (the crate's `WindowVerdict`, with the unit of its `rate_window`).
- `ListAccounts` → `Accounts{accounts, auto}`; broadcast `AccountsUpdated` on every change.
- `SwitchAccount{number, request_id}`, `SetAutoSwitch{enabled, threshold_pct}` (any origin).
- `AddCurrentLogin`, `RemoveAccount{number}`, `SetAccountAlias{number, alias}`, `StartAccountLogin`, `SetAutoSwitchRules{hysteresis, cooldown, strategy}` (local only). No message carries a secret.
- `AccountEvent{kind: switched{from, to, trigger} | all_exhausted{earliest_reset_at} | quarantined{number} | unquarantined{number}}`, broadcast. The fleet verdict ("All accounts blocked {when} until {when}") rides the attention feed (RT-87).

**Native client:** `footer.rs` gains the chip; new `accounts.rs` (model) and `accounts_view.rs` (popover); `settings_view.rs` gains an Accounts tab, hidden off Windows; `notify.rs` shows the toasts under a new Notifications toggle, "Account switches". A client whose `Welcome` says `Remote` shows the admin controls disabled, with "Only on the machine running the daemon".

## Steps

Sub-issues: RT-908 takes the crate (CA.0, CA.2, CA.3); RT-909 the origin and the read-only protocol (CA.1, CA.5 read half); RT-910 the poller (CA.4); RT-911 admin and switch (CA.5 write half, CA.6); RT-912 auto-switch (CA.9); RT-913 the chip (CA.7); RT-914 the Settings page and toasts (CA.8, CA.10); RT-915 login and docs (CA.11, CA.12).

- [ ] **CA.0 Spike: the crate's contract** (RT-908). In `docs/spikes/accounts-crate.md`: (c) a real usage response and its rate-limit headers, to check the transport keeps `Retry-After`. (d) The `claude` CLI's non-interactive login command, and what `claude auth status --json` reports after it, in a scratch `CLAUDE_CONFIG_DIR`.

  (f) A measured check that the daemon and ops-tower's server exclude each other on one real store, through the account lock and the engine lease, the one piece of cross-process behaviour the crate's tests cannot prove from this side. Also: whether `capture_live_login` on a scratch `ClaudePaths` takes the scratch login without touching the live one.

  Rulings the later steps need go into this file's Design.
- [ ] **CA.1 Connection origin** (RT-909). `crates/daemon/src/server/connection.rs` (`AuthKind`, `client_session`), `crates/protocol` (`Welcome.origin`, `#[serde(default)]`). Test: a tailnet connection is refused an admin message that a local one is allowed.
- [ ] **CA.2 Fork the crate** (RT-908). The forked crate at `crates/agents/claude/accounts/` as a workspace member, the lockfile resolved (`jiff`, `proc-table`, `zeroize`, `serde_json` features). A smoke test that `Store::open(AccountsRoot::from_local_app_data(..))` and `read_sequence` work on a temp root.

  `Store::write_refusal()` is checked before every store write and refresh spend; `StoreErrorKind` is `#[non_exhaustive]`, so a wildcard arm, with messages for `AccountReidentified`, `StoreReadOnly`, `UnknownSchema` and `RootUnsafe`. The lock folder exists before `FileLock::acquire`. No fixtures of this repo's own: the crate's tests pin the format.
- [ ] **CA.3 Transports** (RT-908). `accounts/transport.rs`; the daemon's `Cargo.toml` gains reqwest's `blocking` feature; CLAUDE.md's no-direct-API sentence gains this exception. `get_prepaid` stays `Unsupported`. Tests against a local axum mock: a 429 keeps `Retry-After` through `RawUsage`, and a 400 `invalid_grant` body reaches the crate's classifier. Parsing, refresh, the consume lock and quarantine are the crate's.
- [ ] **CA.4 Usage poller** (RT-910). `accounts/driver.rs`, `accounts/snapshot.rs`, `accounts/mod.rs`, hub wiring in `server/`. A `spawn_blocking` task calls `poll_with_refresh(&transport, &ctx, FetchScope::..)` on the crate's `next_delay` and `plan_after_fetch` schedule; the crate persists the cadence and the shared `cache/usage.json`, so ops-tower and the daemon spend one budget.

  The `RefreshContext` is built once (store, `ClaudePaths` from `claude_home`, transports, `Drain`, `ProbeVerdicts`, `LockTiming`, clock). Parking at zero clients stays this repo's rule, except while an engine is live. `UsageTable` becomes `AccountSnapshot`, then `AccountsUpdated`. Shutdown calls `Drain::start` and `wait_idle` with a bound. Tests with a fake clock around the driver, not the cadence maths.
- [ ] **CA.5 Protocol** (RT-909 read half: `Welcome.origin`, `ListAccounts`, `Accounts`, `AccountsUpdated`, `AccountEvent`; RT-911 write half: the admin messages). The messages above in `crates/protocol`, handlers in `server/`, `net.rs` decoding in the native client. `cargo nextest run -p protocol --cargo-profile test-release` stays green.
- [ ] **CA.6 Manual switch** (RT-911). The handler calls `probe_outgoing`, then `switch(&ctx, number, &probe)` in `spawn_blocking`, and maps `SwitchOutcome` to `AccountEvent::switched` and `SwitchErrorKind` to `ActionFailed` codes, `Draining` on shutdown. The add-current-login handler calls `capture_live_login` (`CaptureKind::{Added, Refreshed, Kept}`), local origin only.

  Remove and alias call the fork's `Store::remove_account` and `Store::set_alias`; the origin gate refuses `Remote`. Tests are the handler mapping and the origin gate: the transaction, rollback and ownership classification are the crate's.
- [ ] **CA.7 Footer chip and popover** (RT-913). `footer.rs`, new `accounts.rs` and `accounts_view.rs`. UI spec `tests/ui/ui_accounts.rs`: the chip shows the active account and bars; Switch sends `SwitchAccount`; a `relogin_required` row shows its reason; a stale fetch shows its age; a "store read-only" or "held by another engine" row shows its reason. No paid-fallback state.
- [ ] **CA.8 Settings → Accounts** (RT-914). `settings_view.rs`: list with alias, remove, "Add current login" (with the warning not to `/logout` first, since that can revoke the account being left), auto-switch on/off, threshold, hysteresis, cooldown, strategy; the tab is hidden off Windows; admin controls disabled with a reason on a `Remote` origin. Auto-switch settings go through the crate's settings writer. UI spec in `ui_accounts.rs`.
- [ ] **CA.9 Auto-switch engine** (RT-912). `accounts/driver.rs`: a loop around `Engine::new(owner, pid, warm_since)` and `Engine::tick(&TickInputs{context, collector: &TransportCollector(&usage), processes, switcher: &LiveSwitch, claude_config_dir_set, settings, auto_switch_enabled})`, sleeping on `next_delay` and `sleep_event`. The engine claims and heartbeats the `Lease`; the driver releases it on shutdown and on `halt`.

  `Event`s map to `AccountEvent` and toasts. The engine is off by default. `TickOutcome::Paused{reason}` surfaces as `paused_reason`; `TickOutcome::Halted` (a failed rollback) is a user-visible error. The rules, warm-up, freshen, trigger, ranking, hysteresis, cooldown and quarantine are the crate's. Tests: the driver (a settings change wakes the loop, a `Paused` reason surfaces, drain stops it), not the rules.
- [ ] **CA.10 Toasts** (RT-914). `notify.rs`, `settings_view.rs` (the "Account switches" toggle). UI spec: an `AccountEvent` raises a toast only when the toggle is on.
- [ ] **CA.11 Daemon-driven login** (RT-915). `accounts/login.rs`, `server/`, `settings_view.rs` ("Log in a new account…"): spawns the login found in CA.0 in a scratch `CLAUDE_CONFIG_DIR` as a visible plain session; when it exits logged in, captures it with `capture_live_login` on a `ClaudePaths` for that dir and removes the scratch dir. Local only. Live e2e: `fake-claude` gains a login mode that writes a credential and `.claude.json`.
- [ ] **CA.12 Docs** (RT-915). `docs/architecture.md` (the component and a task-index row), `docs/on-disk.md` (the store is ops-tower's, outside our config dir), the `docs/README.md` glossary checked against the shipped terms; this subplan's durable text promoted and the file deleted.

Phone and remote work outside this wave: MA10 in [mobile-app.md](./mobile-app.md) is the phone's accounts screen over these messages; MA7's pushes carry `AccountEvent`s; a remote native client (Phase 6) gets the chip and popover with no extra work, and the admin controls disabled. Accounts on macOS and Linux waits on ops-tower; the fleet verdict in the attention feed is RT-87.

## Pitfalls to keep

- Never refresh the active account's token outside Claude Code's lock pair, and never spend one refresh token twice: both kill the login. The crate does both; do not bypass it.
- No network call while holding any lock. The crate enforces it; the transports never hold one.
- A token, key or credential byte never reaches a log line, a protocol message or `state.json`.
- `CLAUDE_SECURESTORAGE_CONFIG_DIR` set means the crate refuses to refresh (`RefreshContext.securestorage_dir_set`); the daemon passes it and shows the reason.
- Codex and Cursor sessions, and Claude sessions on another provider (DeepSeek, whose routing variables set their own credential), are untouched by a switch. The chip says which account Anthropic sessions use, not every session.
- An `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` in the daemon's own environment overrides the credential file for every session it spawns; the accounts module detects that and shows it as the reason switching has no effect.
