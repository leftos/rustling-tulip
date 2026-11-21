# Settings › Linear

Design for RT-2024: a per-host Settings page for Linear issue links, with the API key sealed in the host's secret store, an on/off switch, a status block and a Test button. Owner rulings are in the issue; everything below that is not quoted from them is an orchestrator decision, open to the owner.

## Owner rulings

- Key storage: Settings › Linear takes the key and the daemon seals it in Windows Credential Manager (the macOS Keychain on a Mac host), like secret env rows (`docs/env-secrets.md`); `LINEAR_API_KEY` stays a fallback when nothing is saved.
- Default: on when a key is set. A switch on the page, on by default once a key is configured; off stops every Linear request, the underlines and the chips.
- The page shows connection status (whether a key is set, the workspace it reaches, the team keys it recognises, the last error such as a revoked key) and a Test button that checks the key against Linear now and reports the workspace or the error. It is per host, following the Settings host selector. Setting or clearing the key is local-only, like account administration.

## Parts

- RT-2026 (daemon and protocol): the stored key, the switch, the status and test messages. Lands first.
- RT-2027 (native client): the page, and the client's off behaviour. Blocked by the daemon part.
- RT-2028 (daemon, macOS): the login Keychain as the daemon's `keyring-core` store, so a Mac host can hold the key (and secret env rows and summarizer keys). Independent of the other two.

## Protocol

All additive; no protocol bump.

- Feature `FEATURE_LINEAR_SETTINGS = "linear_settings"`, listed by every daemon that has this part. A client offers the page only on a host that lists it.
- `ClientMessage::SetLinearKey { key: Option<Secret>, request_id }`: `Some` saves (replacing any saved key), `None` clears. Local-only: a tailnet client is refused by `host_only_refusal` with `LOCAL_ONLY` ("the Linear key is set on this host's own PC"). A blank key, a key over the store's size limit or a store failure is refused with an `Error` carrying the request id and code `LINEAR_REFUSED`; no message carries the key.
- `ClientMessage::SetLinearEnabled { enabled, request_id }`: open to every origin, like other host settings; it spends nothing and is undone by the same switch.
- `ClientMessage::TestLinear { request_id }`: open to every origin.
- `DaemonMessage::LinearStatus { enabled, key: LinearKeyState, workspace_name: Option<String>, team_keys: Vec<String>, last_error: Option<String> }`: sent at connect (initial state and lag resync, beside `LinearContext`) and broadcast to every client whenever any field changes. It is the echo of both Set messages.
- `LinearKeyState`: `Missing`, `Stored`, `Environment` (no saved key; `LINEAR_API_KEY` is set), `Rejected` (Linear refused the key in use), `Unreachable` (the secret store is out of reach and `LINEAR_API_KEY` is not set), and `#[serde(other)] Unknown`.
- `DaemonMessage::LinearTest { workspace_name: Option<String>, team_keys: Vec<String>, error: Option<String>, request_id }`: the Test's answer; exactly one of `workspace_name` and `error` is set.

## Daemon

- **Store.** One `keyring-core` entry, service `env_secrets::service()`, user `linear/api-key`, written and read like a summarizer key (`crates/daemon/src/summarizer/keys.rs`). It has no index entry, so the unreferenced-secret sweep never removes it.
- **Which key is used.** `RUSTLING_TULIP_LINEAR_OFFLINE=1` wins over everything: no key, no store read, no request. Otherwise the saved key; else `LINEAR_API_KEY` through `user_env::resolve`. A store that cannot be read counts as no saved key, so `LINEAR_API_KEY` still applies; the state reads `Unreachable` only when that variable is unset too. A rejected saved key never falls through to the variable.
- **When it is read.** At start, at once after `SetLinearKey` (then the context is refreshed, the title scan reruns and status and context are broadcast), and on the five-minute tick while no key is in use (the existing late-variable pickup, now reading the store first).
- **The switch.** `state.json` field `linear_enabled: Option<bool>`; a missing field reads on, so every released `state.json` reads on. Linear spends nothing of the user's subscription, so the opt-in rule for paid features does not apply. Off: no Linear request of any kind (issue fetches, the title scan, the refresh tick, context refreshes); the daemon publishes `LinearContext` with no team keys, no workspace and `key_configured: false`, so an older client draws no chips either; a `GetLinearIssues` gets no reply. Turning it on again refreshes the context and reruns the scan.
- **Errors.** `last_error` holds the display text of the last failed Linear request (context, issues or Test) and clears on the next success. HTTP 401 or 403, or a GraphQL error whose `extensions.code` is `AUTHENTICATION_ERROR`, marks the key in use `Rejected`: its hash is remembered in memory (as `summarizer::keys::Rejections` does) and no request is made with it until the key changes or a Test with it succeeds.
- **Test.** Uses the key the daemon would use now, whatever the switch, bypassing the context cache, with `query Test { teams { nodes { key } } organization { name urlKey } }`. No key answers `error: "No Linear API key is set."`; offline answers `error: "Linear is turned off for this daemon (RUSTLING_TULIP_LINEAR_OFFLINE=1)."`. It runs on its own task, like `TestSummary`. A success clears `last_error` and a `Rejected` mark; a rejection sets them.

## Native client

- **Tab.** "Linear", after Dashboard, offered when the Settings host lists `linear_settings`; follows the host selector.
- **Rows.** A switch, "Show Linear issue links". The key: a masked text field with Save and Clear, locked with the note "Set the key on {host} itself." when the Settings host is not this machine or the connection is remote (`accounts_admin_locked`'s rule). Key state words: Missing "No key", Stored "Key saved", Environment "Using LINEAR_API_KEY", Rejected "Linear refused this key", Unreachable "The secure store is out of reach on this host". Status: Workspace (name), Teams (keys, comma-separated), Last error (when set). A Test button and its result line: "Reached {workspace} · teams {keys}" or the error.
- **Off.** `IssueContext` gains `enabled` (default true; an older daemon never sends `LinearStatus`), set per host from `LinearStatus.enabled`. Off: no underline at all, linear.app URLs included; no title chips; an open card closes.
- **No-key card.** "Add a Linear API key in Settings › Linear to see this issue's details." in place of the text naming `LINEAR_API_KEY`.
- **Shot.** `native-shot settings-linear`.

## Steps

- [ ] RT-2026: daemon and protocol
- [ ] RT-2027: native page and off behaviour
- [ ] RT-2028: macOS Keychain store
