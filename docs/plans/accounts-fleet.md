# Accounts fleet status

How the Claude accounts popover and Settings › Accounts show when the fleet runs dry, how long it stays dry and when it returns, and the ops-tower account features rustling-tulip lacks. Mock: [canvases/fleet-dry/index.html](canvases/fleet-dry/index.html), rendered as [shot.png](canvases/fleet-dry/shot.png). ops-tower's own design for a fleet bar is its `docs/plans/canvases/fleet-timeline/index.html`.

## Settled

- **Fleet line** (mock lane A): the run-dry time and the return time each as a date and time with a countdown, and the dry duration, e.g. "Runs dry Sun 8 Nov 12:38 (in 1d22h) · dry for 24h00m · back Mon 9 Nov 12:38 · 7 accounts". Any later spell in the forecast window gets a second line, "Then dry again … for …". All of this comes from `FleetVerdict.blocked_at` and `resumes_at`, already on the wire. The fleet dry duration is `resumes_at - max(blocked_at, now)`.
- **Fleet bar** (mock lane A): a 7-day bar under the fleet line, green while some account has usage left and red for each projected spell when every account is used up, with a mark at now. Hovering a spell shows its start, end and length. Scope is the fleet only; per-account bars (lane B) are out.
- **Times, everywhere the accounts UI shows one** (fleet line, used-up "back", window "resets"): later today, the clock only (`16:33`); the next calendar day, `tomorrow 00:30` (ops-tower's `clockText` in `ui/accounts.js`); the rest of the coming week, weekday, date and clock (`Mon 9 Nov 12:38`); further out, date and clock (`16 Nov 09:00`). Each is followed by its countdown in ops-tower's compact form, `(in 2d22h)`; each dry spell's length reads `dry for 24h00m`, `3d4h`, `45m`.

## Steps

- [ ] One time and duration formatter for the accounts UI (`apps/native/src/accounts.rs`, replacing `clock()`), with a case table of named tests: same day, tomorrow across midnight, six days out, eight days out, a DST change, and durations under an hour, under a day and over a day.
- [ ] The fleet line's new wording from the existing wire fields, in the popover and Settings › Accounts.
- [ ] Fleet spells on the wire: a `gaps` list (start, optional end) on `FleetRow` and `FleetVerdict`, filled from the crate's `WindowVerdict.gaps` (`crates/agents/claude/accounts/src/pace.rs`) in the plugin's mapping (`crates/agents/claude/src/accounts.rs`). Additive, `#[serde(default)]`, no protocol bump.
- [ ] The 7-day fleet bar with its hover, in the popover and Settings › Accounts.

## ops-tower parity, booked separately

The owner picked these from the comparison: a fleet warning on the footer account pill; per-window pace ("runs out in 2h40m" or "lasts to reset"); a list of used-up accounts by soonest return; enabling and disabling an account without removing it; a recent auto-switch events list; per-model limit and extra-usage rows; usage history kept across daemon restarts. Each is its own issue.
