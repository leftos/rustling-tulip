# claude-accounts

The Claude plugin's account store: the managed Claude Code logins, their sealed credentials and configs, the locks that guard them, usage polling, refresh-grant handling, switching the live login, and the auto-switch engine's shared files. The caller supplies the store's root (`AccountsRoot::new`); the crate reads no environment to find it.

## Sealed format

The seal format's identifiers are rustling-tulip's own: the `RTSEAL` magic, the canary text and the `rustling-tulip/accounts/...` DPAPI entropy constants in `src/seal.rs`. They are part of the sealed bytes, so changing them makes every existing credential file unopenable.

## Removing an account

`Store::remove_account(claude, number)` takes the account's consume lock, then the account lock, the order a switch takes them. Under both it first drops the account's refresh stash entries and their manifest rows, under the stash's manifest lock, so none can be adopted for the number. Then it drops the record and its `order` entry from `accounts.json`, keeping `nextNumber` so the number is never given again, and deletes `credentials/<n>.cred`, `credentials/<n>.cred.prev` and `configs/<n>.json` (a file already absent counts as deleted).

`credentials/.consume-<n>.lock` stays: deleting it while another process waits on it would let two processes lock different files for one number. The active account, and the account Claude Code is logged in as now (`~/.claude.json`'s `oauthAccount`), are refused (`StoreErrorKind::ActiveAccount`): switch away first.

## Aliases

`Store::set_alias(lock, number, alias)` stores the alias trimmed, removes it for `None` or a blank alias, and returns `false` when nothing changes. An alias over 64 characters once trimmed, or one holding a control, bidi-control or other invisible character (the same rule as a model name), is refused (`StoreErrorKind::InvalidAlias`).

## Settings

`Settings` is the store's `settings.json` (`AccountsRoot::settings`): an `autoswitch` section (`enabled`, which is false when the file or the key is missing since auto-switch is opt-in, `threshold`, `strategy`, `models`, `intervalSeconds`, `cooldownSeconds`, `hysteresisPct`, `unhealthyTicks`) and a `fleet` section (`rateWindowMinutes`: 30, 60, 120 or 180). `Settings::read` gives the defaults for a missing file and refuses an unknown field, a wrong type or a value out of range, naming the field. `Settings::write` checks the ranges first, then writes under `.settings.lock` through a temp file in the root, always with both sections. `AutoSwitchSection::to_engine` gives the engine's `AutoSwitchSettings`.

## Platforms

The locks work on 64-bit Windows and macOS (on macOS the store's file locks are `flock` on owner-only lock files, and Claude Code's lock directories are opened without following a link); Linux has none, so every lock there reports `Unsupported`. DPAPI sealing is Windows-only; off Windows the store's files are owner-only and unsealed. On macOS the live credential is still `.credentials.json`: the Keychain credential comes with RT-962.
