# Dashboard mock: rebuild recipe

The mock for RT-28 (the Dashboard): `index.html`, three layout lanes switched by the tabs at its top. The owner chose lane A, dense rows. It loads Geist and JetBrains Mono from `apps/native/assets/fonts/` by relative path, so it opens from this folder in a checkout.

Its screenshots (about 1.8 MB) are not committed. To rebuild them beside the page:

```powershell
uv run --no-project --with playwright python -I docs/plans/canvases/dashboard/shoot.py docs/plans/canvases/dashboard/index.html
```

That writes `lane-a.png`, `lane-b.png`, `lane-c.png` (1640×1120 viewport), `lane-a-full.png`, `lane-b-full.png` (grown to each view's height) and `lane-c-permission.png` (a session waiting on a permission prompt). Run `uv run --no-project --with playwright python -m playwright install chromium` first on a machine without Playwright's Chromium.

A self-contained review copy, fonts embedded, comes from `uv run --no-project python ~/.claude/tools/mock-embed/embed_media.py docs/plans/canvases/dashboard/index.html .tmp/mocks/dashboard-review`.
