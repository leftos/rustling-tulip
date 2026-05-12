---
name: add-protocol-message
description: Add a new message to the rustling-tulip wire protocol. Walks the user through the 4-file pattern (Rust enum -> daemon handler -> TS type -> TS dispatcher) and enforces the project's invariants (snake_case tags, data_b64 for binary, additive-versus-breaking change rules).
---

# Add a protocol message

Use this skill when the user wants to add a new daemon<->client message, or extend an existing message struct. It codifies the project's hand-mirrored protocol pattern so nothing is missed.

## Interview the user first

Before writing any code, gather:

1. **Message name** in PascalCase (e.g. `ReloadConfig`, `SnapshotState`). It will be `rename_all = "snake_case"` on the wire — confirm the snake_case form is what they want.
2. **Direction**: one of
   - client -> daemon (a request)
   - daemon -> client (a notification or response)
   - both (a request with a paired response — confirm the response variant name)
3. **Fields**: each field's name (snake_case), Rust type, and whether it's optional. Flag any binary data — it must be `data_b64: String`, not `Vec<u8>` or raw bytes.
4. **Protocol version impact**: walk the user through the rules below and confirm whether their change is additive (no bump) or breaking (bump).

Don't proceed until all four are settled.

## Protocol version rules

These come from `CLAUDE.md` — re-state them so the user is making the call deliberately:

- **Additive (no bump)**: new variant on an existing tagged enum, new field on a struct with `#[serde(default)]`, new nested-enum variant that joins an enum already carrying `#[serde(other)] Unknown`.
- **Breaking (bump)**: renaming a field, removing a variant, changing a variant's wire tag, changing semantics of an existing field.

When in doubt, treat it as breaking. If the user picks "bump", also remind them to append the new version to `SUPPORTED_PROTOCOL_VERSIONS` so older clients can still negotiate.

## The 4-file edit pattern

Edit in this order. Do not skip the order — the Rust enum is the source of truth and downstream changes depend on the field names you chose.

### 1. `crates/protocol/src/lib.rs`

Add the variant inside `pub enum ClientMessage` and/or `pub enum DaemonMessage` (whichever matches the direction).

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    // ... existing variants ...
    YourNewMessage {
        // snake_case fields, default-when-additive on new optionals
        repo_id: String,
        #[serde(default)]
        force: bool,
    },
}
```

Guardrails:
- If the variant has any nested enum that may grow over time, give that enum its own `#[serde(other)] Unknown` arm from day one.
- If the field is binary, use `data_b64: String` and document the encoding in the Rust doc comment.
- Add a `///` doc comment explaining what the message is for and when the peer should emit it.

### 2. `crates/daemon/src/server.rs`

For a `ClientMessage` variant, add an arm to the dispatch `match` in the receive loop (around the existing `ClientMessage::ListRepos`, `ClientMessage::AddRepo` arms). For a `DaemonMessage` variant, add the code that constructs and sends it from wherever the new behaviour fires.

Guardrails:
- If the handler does I/O or anything slow, `tokio::spawn` it — never block the WS receive loop.
- Use `tracing::{info, warn, error, debug}` for logging, never `println!`.
- Return errors as `DaemonMessage::Error { message, .. }` on the same connection — never panic, never `unwrap()`.

### 3. `apps/tauri-app/src/types.ts`

Mirror the variant in the `ClientMessage` or `DaemonMessage` union type. The `type` tag is the snake_case form of the Rust variant name. Field names stay snake_case (the frontend does not camelCase wire shapes — verify against existing variants if uncertain).

```typescript
export type ClientMessage =
  // ... existing variants ...
  | {
      type: "your_new_message";
      repo_id: string;
      force: boolean;
    };
```

Mapping rules:
- `String` -> `string`
- `Option<T>` -> `T | null` (never `T | undefined`)
- `Vec<T>` -> `T[]`
- `bool` -> `boolean`
- `u32`/`u64`/`i32`/`f64` -> `number`
- `data_b64: String` -> `data_b64: string` and add a `// base64-encoded` comment

### 4. `apps/tauri-app/src/api.ts`

If the new message is a `DaemonMessage`, add a case to the receive dispatcher (`handleMessage` or equivalent) that does something with it — either updates state, surfaces a toast, or logs and drops. A `DaemonMessage` variant that has no handler is a silent bug.

If the new message is a `ClientMessage`, add a typed helper that constructs and sends it (look at `requestSpawnConfig`, `loadScrollback`, etc. for the pattern). Helpers go alphabetically near related helpers.

## After the edits

1. Run `cargo build` to confirm the Rust side compiles.
2. Run `pnpm typecheck` in `apps/tauri-app/` to confirm the TS side compiles.
3. Invoke the `protocol-sync-checker` subagent to verify there are no orphan variants, missing dispatcher cases, or field-name typos. The subagent reads both sides and reports drift with file:line precision.
4. Only after the subagent gives a clean report, suggest the user run `cargo clippy --all-targets -- -D warnings` and `pnpm tauri dev` to smoke-test the round trip.

## Common mistakes to catch

- Forgetting the `#[serde(default)]` on a new optional field that was claimed as additive — without it, older peers will fail to decode.
- Using `camelCase` field names in TS to match local conventions. The wire is snake_case on both sides.
- Adding a `DaemonMessage` variant but no handler in `api.ts` — the message arrives and is silently dropped.
- Writing `T | undefined` instead of `T | null` in the TS mirror. JSON.parse will produce `null` for `Option::None`, and `undefined` will not type-narrow.
- Treating a field-rename as additive. Rename is always breaking — bump the version.
