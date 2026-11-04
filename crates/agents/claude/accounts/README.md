# claude-accounts

The Claude plugin's account store: the managed Claude Code logins, their sealed credentials and configs, the locks that guard them, usage polling, refresh-grant handling, switching the live login, and the auto-switch engine's shared files. The caller supplies the store's root (`AccountsRoot::new`); the crate reads no environment to find it.

## Sealed format

The seal format's identifiers are rustling-tulip's own: the `RTSEAL` magic, the canary text and the `rustling-tulip/accounts/...` DPAPI entropy constants in `src/seal.rs`. They are part of the sealed bytes, so changing them makes every existing credential file unopenable.

## Platforms

Windows today: the locks and DPAPI sealing are Windows-only, and elsewhere the locks report `Unsupported`. macOS support (its locks and the Keychain) is RT-916.
