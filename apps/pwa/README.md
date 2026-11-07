# rustling-tulip phone app

The web app a phone opens over the tailnet: the installable shell the daemon serves from its `pwa` folder (`crates/daemon/src/server/pwa_static.rs`), built with Vite and plain TypeScript. The service worker in `src/sw.ts` is hand-written; there are no runtime dependencies.

## Scripts

Run from this folder, or from the repo root as `pnpm --dir apps/pwa <script>`:

| script              | what it does                                                                                                                  |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `dev`               | Vite dev server                                                                                                               |
| `build`             | type-check both projects, then write `dist/`                                                                                  |
| `typecheck`         | `tsc --noEmit` for the page, then for the service worker (`tsconfig.sw.json`, which type-checks it against the WebWorker lib) |
| `lint`              | oxlint, with the typescript, import and unicorn plugins                                                                       |
| `fmt` / `fmt:check` | oxfmt, rewrite or check                                                                                                       |
| `test`              | vitest, one pass                                                                                                              |

## Build output

`dist/` holds `index.html`, `sw.js`, `manifest.webmanifest` and the icons at the top level under their stable names, and the hashed page assets under `assets/`. The daemon serves that folder from beside its install (`pwa`), or from the folder `RUSTLING_TULIP_PWA_DIR` names for a dev run: hashed assets are cached immutably, everything else is revalidated.

The folder reaches the daemon in two ways. For a dev run, `.\rt.ps1 build -Pwa` runs the install and build above and replaces `target\<profile>\pwa` with `dist/`. A release's `.\rt.ps1 installer` does the same and ships it as `pwa` in the install folder, which an update replaces by rename.

The icons are resized from `tools/installer/icon/icon-1024.png`; regenerate them when that artwork changes.
