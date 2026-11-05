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

The locks work on 64-bit Windows and macOS (on macOS the store's file locks are `flock` on owner-only lock files, and Claude Code's lock directories are opened without following a link); Linux has none, so every lock there reports `Unsupported`. DPAPI sealing is Windows-only; off Windows the store's files are owner-only and unsealed. On macOS Claude Code keeps its live login in the login Keychain, and so does the crate when the caller builds its `ClaudePaths` with `ClaudePaths::with_keychain` (the plugin does on macOS; the crate reads no environment, so the caller names the service, the account and the `security` tool).

The live login on macOS is the generic-password item of service `Claude Code-credentials` (suffixed with `-` and the first 8 hex digits of the SHA-256 of the config dir in NFC when `CLAUDE_CONFIG_DIR` is set, and `CLAUDE_SECURESTORAGE_CONFIG_DIR` winning over it when set, an empty one meaning the unsuffixed name) and account `$USER` (else the OS user name, else `claude-code-user`), read and written through `/usr/bin/security`, never one found on `PATH`. Every read of the live login takes the item, and `.credentials.json` only when there is no item.

A write goes to the item, with the value hex-encoded on `security -i`'s stdin so it stays out of the process list (in the arguments, where it shows, only past `security -i`'s line length), is read back and compared, then rewrites an existing `.credentials.json` with the same bytes, never creating one, so a running Claude Code that watches the file notices the switch sooner. A rollback restores, and removes, the item it wrote.

The item holds only printable ASCII, since `security -w` prints any other value hex-encoded: a login whose bytes are not all printable (a trailing newline, pretty-printed JSON) goes into the item as its compact JSON, and a login that is not JSON, or still not printable once compact, is refused with nothing written.

Each `security` call is bounded to 5 s; a call that fails for another reason than "no such item" (a locked or refusing Keychain) or does not answer is an error naming its exit code (`ClaudeFileError::CredentialStore`), never a fallback to the file. Claude Code caches its Keychain read for about 30 s, so a switch's outcome says so in `SwitchOutcome::takes_effect_within` (`KEYCHAIN_CACHE`), and `None` where Claude Code reads the file afresh.

The account store stays an owner-only folder of files on macOS (mode 0600 files under the 0700 root), not Keychain items per account: its files never leave the owner's account, and an item per account would be a second store to keep in step with the folder.

## macOS smoke run

`tests/smoke_macos.rs` (macOS only, ignored by default) runs the Keychain path against the real `/usr/bin/security`: it captures two logins from a Keychain item with `capture_live_login`, switches to the first and back to the second with `switch`, and checks after each switch that the item holds that login's bytes and that the store lists both accounts. The profile endpoint and the credentials come from the refresh tests' fakes (`tests/refresh/support.rs`).

It touches only a throwaway keychain and a scratch service. The test creates `smoke.keychain-db` in a fresh temp folder, unlocks it, and runs every call through a `security-smoke` wrapper. The wrapper appends the keychain's path to each call's arguments, and to each command line `security -i` reads on its stdin. The item's service is `Claude Code-credentials-smoke-` plus 8 random hex digits, and its account is `rt-smoke`. At the end, from a drop guard that also runs after a failed assertion, the test does three things. It removes the item from the throwaway keychain. It searches the default keychain list for the scratch service with `/usr/bin/security` itself, where any item it finds is deleted and fails the test. Then it deletes the keychain by its path.

Run it from Windows on the Mac mini builder:

```powershell
& .\rt.ps1 mac test -- --package claude-accounts --test smoke_macos -- --ignored
```

The first `--` ends `rt.ps1`'s own parameters, since PowerShell would otherwise bind `--test` to `-Test`. The rest goes to `cargo nextest run`. The run's result line:

```text
     Summary [   0.750s] 1 test run: 1 passed, 0 skipped
```
