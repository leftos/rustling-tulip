# Store test fixtures

`accounts-root/` is a hand-written account store (the account store's layout under its root) holding fake values only: the emails are `example.com` addresses and the UUIDs are made up. Nothing here was copied from a real store.

It holds no credential file: a sealed credential belongs to one Windows user's DPAPI keys and cannot be committed, so each test writes account 1's credentials through `Store` in its setup.

`accounts.json` holds `futureTopLevel` and account 1's `futureField`, which stand for keys a later version may add: every write keeps them.

The tests copy `accounts-root/` into a scratch folder under `CARGO_TARGET_TMPDIR` before each test, so the fixture itself is never written.
