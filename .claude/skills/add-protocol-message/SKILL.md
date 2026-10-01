---
name: add-protocol-message
description: Add a new message to the rustling-tulip wire protocol. Walks the user through the 3-step pattern (Rust enum -> daemon handler -> native client handling) and enforces the project's invariants (snake_case tags, data_b64 for binary, additive-versus-breaking change rules, protocol 22 kept decodable for the installed Tauri app).
---

# Add a protocol message

Use this skill when the user wants to add a new daemon<->client message, or extend an existing message struct. It codifies the project's protocol pattern so nothing is missed.

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

The daemon keeps protocol 22 in `supported` while the installed Tauri app (the `tauri` branch) is in use, so a change must leave every v22 message decodable. The `v22_compat` tests in `crates/protocol` fail if it does not.

## The 3-step edit pattern

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

### 3. Native client handling (`apps/native/src`)

The native client uses the Rust types from `crates/protocol` directly; there is no mirror to update.

- A `DaemonMessage` variant reaches the view through `net.rs` (`on_daemon_message` emits `NetEvent::Message`) and is dispatched in `RootView::on_message` (`apps/native/src/lib.rs`). Add the arm there that updates state or surfaces it. A variant the client should act on but has no arm for is silently ignored.
- A `ClientMessage` variant is sent from the view with `self.send(ClientMessage::YourNewMessage { .. })`, which forwards it to the network thread as `NetCommand::Send`.
- Add a UI spec module (`apps/native/tests/ui/ui_<name>.rs`, with its `mod` line in `tests/ui/main.rs`) that drives the round trip against the scripted fake daemon.

## After the edits

1. Run `cargo build` to confirm the workspace compiles.
2. Run `cargo test -p protocol v22_compat` to confirm the change keeps protocol 22 decodable.
3. Run `cargo test -p protocol` and the native client's UI spec that covers the message (`cargo test -p rustling-tulip-native --test ui ui_<name>`).
4. Run `cargo clippy --all-targets --all-features -- -D warnings`.

## Common mistakes to catch

- Forgetting the `#[serde(default)]` on a new optional field that was claimed as additive — without it, older peers will fail to decode.
- Adding a `DaemonMessage` variant but no arm in the native client's `on_message` — the message arrives and is silently dropped.
- Changing a field or variant that a v22 message uses: the installed Tauri app still speaks 22, and `v22_compat` catches it.
- Treating a field-rename as additive. Rename is always breaking — bump the version.
