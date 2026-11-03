# Compact Dashboard layout mock: rebuild recipe

The mock that settled RT-773's layout: three lanes, with the same 20-session fleet at 1588 by 860. The owner chose **A, stacked lanes**: one full-width table with the repo on the left spanning its sessions, one row per session, then a cell of agent, subagent and shell chips that wraps.

`src.html` is the page. Its fonts stay out of git, so the review copy is rebuilt into the untracked `.tmp/mocks/dashboard-compact/`.

## Rebuild

From the repo root:

```bash
mkdir -p .tmp/mocks/dashboard-compact/fonts
cp docs/plans/canvases/dashboard-compact/src.html .tmp/mocks/dashboard-compact/
cp apps/native/assets/fonts/Inter/Inter-{Regular,Medium,SemiBold,Bold}.ttf .tmp/mocks/dashboard-compact/fonts/
cp apps/native/assets/fonts/JetBrainsMono/JetBrainsMono-{Regular,SemiBold}.ttf .tmp/mocks/dashboard-compact/fonts/
```

Then write `.tmp/mocks/dashboard-compact/index.html` with the fonts inlined as `data:` URIs, replacing each `url(fonts/<file>)` in `src.html` with `url(data:font/ttf;base64,<the file's base64>)`. A few lines of Python do it.

Render it headless and cut it into strips:

```bash
npx --yes playwright screenshot --browser=chromium --full-page --viewport-size=1640,1000 \
  --wait-for-selector=body.ready "file:///X:/dev/rustling-tulip/.tmp/mocks/dashboard-compact/index.html" \
  .tmp/mocks/dashboard-compact/dashboard-compact.png
uv run --project ~/.claude/tools/image-cut python -m image_cut split .tmp/mocks/dashboard-compact/dashboard-compact.png --height 1000 --overlap 30
```
