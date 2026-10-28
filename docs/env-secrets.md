# Secrets in environment rows

A secret typed into the spawn dialog's environment rows is sealed: the daemon moves its value into Windows Credential Manager and keeps only a reference in every stored file and every echo. How the pieces fit: [architecture.md](./architecture.md); the dialog's side: [native-client.md](./native-client.md). Terms (secret reference, sealing, secret row, env reference) are in the [glossary](./README.md#glossary).

## Rows and references

An env row is `(key, value)` in `SpawnRequest.extra_env` and `SpawnConfig.extra_env`; the wire shape never changed. A value is one of three things, parsed by `protocol::env_rows::reference`:

- A **literal**, passed to the child as typed.
- An **env reference**, `${env:NAME}`: resolved at spawn from the daemon's environment, then the user scope (`HKCU\Environment`), then the system scope (`HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment`, expandable values expanded) by `user_env::resolve`. Only the reference is stored and echoed. An unset name refuses the spawn, naming the variable and the three places looked in.
- A **secret reference**, `${secret:<id>}`, where `<id>` is 32 lowercase hex characters (a UUID v4, simple form). It stands for a value held in Credential Manager.

Existing variables are reused, not retyped: `ClientMessage::ListEnvNames` is answered by `DaemonMessage::EnvNames` with the variable **names** and their scopes (never values), and the spawn dialog offers them when `$` is typed or Down pressed, writing `${env:NAME}` on a pick.

## Sealing rule

A row is sealed when its value is a literal (not an env or secret reference), non-empty, and either:

- its key has a whole `_`-delimited segment `KEY`, `TOKEN`, `SECRET` or `PASSWORD`, in any case (`protocol::env_rows::is_secret_key`: `ANTHROPIC_API_KEY` and `db_password` match, `KEYBOARD`, `MONKEY_BUSINESS` and `APIKEY` do not); or
- the client listed its key in `SpawnRequest.secret_env_keys` (the dialog's per-row Secret toggle).

The daemon applies the name pattern itself, so a request from any client is covered; the toggle can add a key the pattern misses but cannot exempt one it matches.

`spawn_session` calls `env_secrets::seal_rows` before `SpawnConfig::from_request`, so the captured config, the session record, `sessions/<id>/meta.json`, `state.json`'s `last_spawn_config`, the `Repos` / `Workspaces` broadcasts, `GetSpawnConfig` replies and the history entry all carry the reference.

`secret_env_keys` is not part of `SpawnConfig`: once sealed, the reference itself marks the row.

A value Credential Manager cannot take (over its credential size limit) refuses the spawn with "`<KEY>`'s value is too long to save securely; use `${env:<KEY>}` instead."

## The store

`crates/daemon/src/env_secrets.rs`:

- Values live in Windows Credential Manager through `keyring-core` and `windows-native-keyring-store` (default features off, so no `search`). The service is `rustling-tulip`, or `rustling-tulip:<first 8 hex of the SHA-256 of the config dir path>` while `RUSTLING_TULIP_CONFIG_DIR` is set, so an isolated daemon (the e2e tier) never shares entries with the user's own. The user name is `env/<KEY>/<id>`, so an entry can be recognised and deleted in Credential Manager.
- The Dashboard summarizer's provider keys share the service but have their own fixed user name, `summarizer/<provider id>`, and no index entry, so the unreferenced-secret sweep never touches them (`crates/daemon/src/summarizer/keys.rs`).
- `<config dir>/env-secrets.json` indexes `{id, key, created_at}` per stored value, never the value, written through a temp file and rename (`secret::write_private`) under one lock covering every read-modify-write. The index is how the daemon knows which ids exist.
- `seal` reuses an indexed id with the same key and an equal stored value, else stores a new id; one entry per distinct value however many spawns use it. `open` reads a value back as a redacting `user_env::Secret`; `delete` removes the entry and its index line. `Debug` on everything prints ids and keys only.

## Resolving

`resolve_env_refs` in `server/spawn.rs` resolves both reference kinds for the child. A missing secret (deleted in Credential Manager, or cleaned up) refuses, never spawns without the variable: "A saved secret is missing", "`<KEY>`'s saved value is no longer in Windows Credential Manager."

The `SpawnSession` pre-check refuses it before a checkout confirm and before any git work. Duplicate, Restart, Resume, Recover and Launch last all re-resolve, so each fails the same way until the user types the value again.

## Existing files and cleanup

At startup, before orphan recovery reads the sidecars, the daemon seals every literal secret row in `state.json`'s `last_spawn_config`s, every `sessions/<id>/meta.json` and every `history/<id>.json` (`main.rs`: `seal_stored_secrets`, `prune_and_seal_history`), rewriting each file that changed. Only the name pattern applies, since old files have no toggle, and other rows (`RUST_LOG`) are untouched.

A store failure leaves that file as it was and logs a `warn!` naming the file and key, never the value; the next start tries again. History is pruned before it is sealed, so a value only an expired entry named never reaches the store.

After that, `env_secrets::collect_unreferenced` deletes every indexed id that no `last_spawn_config`, sidecar or history entry references and that is older than one hour, from the store and the index. An entry therefore outlives its last reference until the next daemon start, and a spawn that fails after sealing leaves an id the next sweep removes. The hour covers a spawn sealed moments before a restart.

A start that cannot read every source (an unreadable folder or history file, a sidecar from a future version, a `state.json` that failed to parse) skips the sweep with one `warn!` naming it, so an id still in use is never deleted.

## What is and isn't protected

Protected: the value is never written to `state.json`, `meta.json`, `history/`, the logs or the index, and never sent to a client in `Repos`, `Workspaces`, `SpawnConfigReply` or `SessionHistory`; those carry the reference.

Not protected:

- The value still reaches the tracer's and the child's environment, as `${env:NAME}` values and DeepSeek's key do; any same-user process can read a process's environment.
- Scrollback, which may hold anything a session printed, is not scrubbed.
- A client that learns a reference can spawn with it and so gets the value into a child. That is no more than the auth token already allows (any authenticated client can spawn a shell and read the user's Credential Manager or environment itself).
- A secret under a name the pattern misses stays literal unless its row's Secret toggle is on (its key is in `secret_env_keys`).

## Protocol

All additive, no bump: `SpawnRequest.secret_env_keys` (default empty, skipped when empty), `ListEnvNames` / `EnvNames`, and the `protocol::env_rows` helpers shared by the daemon and the native client so the pattern and the reference parser cannot drift.
