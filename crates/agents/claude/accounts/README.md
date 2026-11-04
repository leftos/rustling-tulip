# claude-accounts

The Claude plugin's account store: the managed Claude Code logins, their sealed credentials and configs, the locks that guard them, usage polling, refresh-grant handling, switching the live login, and the auto-switch engine's shared files. The caller supplies the store's root (`AccountsRoot::new`); the crate reads no environment to find it.

## Origin

Forked from ops-tower's `crates/claude-accounts` at `59dc25f`, with ops-tower's `d80c605` and `1f3a225` applied on top. rustling-tulip owns it from here: nothing tracks ops-tower, and it has no dependency on ops-tower.

## Sealed format

The seal format's identifiers are kept as ops-tower wrote them: the `OTSEAL` magic, the canary text and the `ops-tower/claude-accounts/...` DPAPI entropy constants in `src/seal.rs`. They are part of the sealed bytes, so a store copied in from ops-tower opens for the same Windows user on the same machine. Changing them makes every existing credential file unopenable.

## Platforms

Windows today: the locks and DPAPI sealing are Windows-only, and elsewhere the locks report `Unsupported`. macOS support (its locks and the Keychain) is RT-916.
