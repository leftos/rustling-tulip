# UI language: the layer-separation mock (RT-581)

## Question

The owner: "Everything bleeds together in the app's current theming choices. For example the context menu that shows me add repo completely blends into the background and I can't tell where the context menu ends and the repo cards begin. This is just one example. We need better UI language throughout the app."

The mock showed real native-shot captures of four views (`menu`: the sessions panel's ⋯ menu over the selected session row, plus a toast; `main`; `connections`; `settings`) in today's look and three directions, side by side.

## Options

- **Current**: the menu fill `#232129` equals the selected row's, so the menu has no edge against it (0 points of OKLab L between them).
- **A, Elevation**: each layer a clear step lighter (sidebar `#15141a` < session card `#1e1d24` < menu `#433f4e` < modal `#34313d`), a 1px top highlight on floating layers, session rows as soft cards, an accent-tinted selected row.
- **B, Outline**: today's fills, with a bright `#8e8a9c` edge on menus and modals, an outlined box on every session row, the selected row shown by an accent edge, and strong shadows.
- **C, Contrast**: a near-black ground (`#08070a`) and sidebar (`#0d0c10`), much lighter floating layers (menu `#3d3a47`, modal `#2a2832`), a rule between every menu group, session rows as cards (`#16151b`), an accent-tinted selected row, a light primary button (`#e4e0da`), and the accent kept for focus and selection.

Every direction kept body text at WCAG AA (4.5:1) or better on each surface it changed.

## Answer

**C, Contrast** (owner ruling). Recorded on RT-581 in Linear.

## Rebuild

The shots are not committed (about 1.5 MB). The commit that adds this file also holds the mock scaffolding: `apps/native/src/palette.rs` selects each direction's token values at compile time from `RT_MOCK_THEME` (`current`, `a`, `b`, `c`), and `apps/native/examples/shot.rs` has the `menu` view. To rebuild the shots, check out that commit and run, from the repo root:

```bash
bash docs/design/ui-language/shoot.sh "current a b c" "menu main connections settings"
```

It writes `.tmp/shots/<direction>-<view>.png`. The commit after it builds C as the only theme and removes the selector.
