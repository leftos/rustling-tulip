# ops-tower absorption mocks: rebuild recipe

The mocks for RT-464 (absorb all of ops-tower's functionality), one folder per feature group, each an `index.html` with its layout lanes switched by tabs at its top, and a `shoot.py` that renders them. They reuse the Dashboard mock's look (`../dashboard/index.html`) and load Geist and JetBrains Mono from `apps/native/assets/fonts/` by relative path, so they open from a checkout.

The owner's picks, recorded on RT-464:

| Folder | Feature group | Chosen lane |
|---|---|---|
| `ot-dashboard-additions/` | ops-tower's Dashboard and Agents views added to the Dashboard (attention band, accounts card, Linear questions, two-hour activity bars with gate ticks, dispatch rows) | B, by repo lane, as the default |
| `ot-plan-board/` | Linear plan board and the daily briefing | B, a Plan tab |
| `ot-gates/` | gate slots, queues, runs and verdicts | A, a rail view plus a footer strip |
| `ot-accounts/` | Claude logins, usage and pace, auto-switch, paid fallback | A, footer chip and popover plus Settings → Accounts |
| `ot-costs/` | spend per session, repo and day | A, the subscription-share model plus a Costs tab |

ops-tower's LAN pairing has no mock: the owner dropped it for the tailnet listener.

The screenshots (several MB) are not committed. To rebuild one folder's PNGs beside its page:

```powershell
uv run --no-project --with playwright python -I docs/plans/canvases/ops-tower-absorb/<folder>/shoot.py docs/plans/canvases/ops-tower-absorb/<folder>/index.html
```

Run `uv run --no-project --with playwright python -m playwright install chromium` first on a machine without Playwright's Chromium.

A self-contained review copy, fonts embedded, comes from `uv run --no-project python ~/.claude/tools/mock-embed/embed_media.py docs/plans/canvases/ops-tower-absorb/<folder>/index.html .tmp/mocks/<folder>`.
