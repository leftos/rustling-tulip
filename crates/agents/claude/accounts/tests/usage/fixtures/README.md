# Usage test fixtures

`accounts-root/` is a hand-written account store holding fake values only: the emails are `example.test` addresses and the UUIDs are made up. It holds no credential file: a sealed credential belongs to one Windows user's DPAPI keys and cannot be committed, so each test writes accounts 1 and 2's fake credentials through `Store` in its setup (tokens `fake-access-one`, `fake-refresh-one`, `fake-device-one` and `fake-access-two`, `fake-refresh-two`, with `expiresAt` in 2100).

`accounts-root/cache/usage.json` is a usage table of the account store at `schemaVersion` 3 (`USAGE_SCHEMA_VERSION`), its rows shaped as `UsageTable::reserve` and `record` write them: account 1 holds a successful fetch at epoch 1790000000 (2026-09-21 14:13:20 UTC) with a poll plan and a per-model `scoped` window, account 2 a 429 failure in backoff.

`futureRowKey` and `futureTopLevel` stand for keys a later version may add. Nothing here was copied from a real store.

`scoped-spend.json` pairs a usage-endpoint answer with the `lastGood` cswap's `oauth.build_usage_result` makes of it, less the `countdown` and `clock` display strings cswap recomputes from `resets_at`. The answer follows cswap's `tests/test_oauth.py` (`test_scoped_per_model_limits`, `test_extra_usage_complete`): per-model limits arrive in `limits[]` as `weekly_scoped` entries and become `scoped`, and `extra_usage` in cents becomes `spend` in currency units. This crate's `lastGood` also keeps the block's `extra_usage` switches, which cswap's does not: `enabled` (false when the block leaves `is_enabled` out), `user_disabled` and `spend_limit_reached` only when the block sends them, and `disabled_reason_set` only when it has a `disabled_reason` key. This answer sends `is_enabled` alone, so its `lastGood` holds `{"enabled": true}`.

`prepaid.json` holds two prepaid-credits answers in the shape the live endpoint gave (`funded`: `amount` 102003 minor units of USD at exponent 2; `unfunded`: `amount` 0 with a null currency and balance), and an uncapped `extra_usage` block as the usage endpoint gave it (`extraUsageUncapped`: enabled, `monthly_limit` null, `used_credits` 3164.0 at 2 decimal places). The figures follow the live replies; nothing in them names an account or organization.

The tests copy `accounts-root/` into a scratch folder under `CARGO_TARGET_TMPDIR` before each test, so the fixture itself is never written.
