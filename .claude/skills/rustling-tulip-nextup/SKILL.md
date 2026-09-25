---
name: rustling-tulip-nextup
description: Profile for the user-level `nextup` skill in the rustling-tulip repo — loaded by `nextup` at its step 0 for this project's plan convention, agents, gates, docs map and landing path. Not a loop of its own; invoke `/nextup`.
---

# rustling-tulip profile for `nextup`

The generic loop is the user-level `nextup` skill; this file supplies only what is rustling-tulip-specific.

## Plan and tracker

- Index: `docs/plan.md`, section `## Open`. **Current focus: native client** outranks everything; the sections below it follow top to bottom. The focus section's single line points into `docs/plans/native-client.md`: the queue is that file's first phase with unchecked items (Phase 1 is `P1.1`–`P1.8`, in dependency order). Feature scope for each item: the parity sections it names in `docs/plans/native-client-parity.md`.
- A later phase in `native-client.md` is one coarse line until it becomes current. When the current phase's items are all ticked, the next phase is a *design* item: split it into brief-sized `P<n>.<m>` items from its parity sections, and put the split to the user in the decision round before anything is dispatched.
- Siblings: none.
- Pre-loop hooks: none.
- Finished-item convention: **tick the line** (`- [x]`), never delete it. Tick the parity lines the item delivered in `native-client-parity.md` in the same commit. A finished subplan is `git mv`'d to `docs/plans/completed/` in the shipping commit; follow-ups left over from it go into a new plan, never back into the moved one.
- Tracker: `gh issue list --repo leftos/rustling-tulip --state open --json number,title`. No triage skill; place issues by the step-0 rule. A bug in the Tauri app is still taken (it is frozen to bug fixes, not closed).
- Hotspots (two items touching one wait on each other): `crates/protocol/src/lib.rs`, `crates/daemon/src/server.rs`, and in the native client its root view and app-state module (whatever P1.1 names them; P1.4–P1.8 all mount into them).

## Rulings every brief carries

- **Tauri freeze** (user, 2026-09-23): `apps/tauri-app` takes bug fixes only. A new feature lands in the native client alone. An item that needs a protocol change still updates the TS mirror (`apps/tauri-app/src/types.ts`, `api.ts`), because the `protocol-mirror` prek hook and the running Tauri app both depend on it until cutover.
- **Protocol:** additive changes only, the `add-protocol-message` skill's 4-file pattern, no `protocol-version.json` bump for additive changes (CLAUDE.md "When to bump").
- **Testable core, thin view:** state that a feature adds (sidebar tree, split tree, tab list, key mapping, span building) lives in plain-Rust modules with unit tests; the GPUI view only renders it and forwards events. The proving command is that module's `cargo test -p <crate> <filter>`, red first.

## Agents and gates

- Explore: `Explore`. Protocol drift: `protocol-sync-checker` after any `crates/protocol` change. Rust design second opinion: `oracle`.
- Reviewers: `code-review` for every item; `protocol-sync-checker` as well when `crates/protocol` changed.
- Gates, each wrapped as `cmd > .tmp/<name>.log 2>&1; rc=$?; tail -n 20 .tmp/<name>.log; (exit $rc)` from the worktree root:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` (what `.\rt.ps1 clippy` and prek run)
  - `cargo test -p <crate>` for each touched crate
  - `cargo deny check` when `Cargo.toml` or `Cargo.lock` changed
  - `pnpm --dir apps/tauri-app typecheck` when `apps/tauri-app/src` changed
- UI that only a person can verify: when a native item's visible result can't be proved by a test, it lands on green gates, and the checkpoint lists it under "hand-test" with what to look at. A **visual fix** (something already landed that looks wrong) is not committed until the user confirms it by hand-testing; edit, ask, then squash (memory: commit-only-after-confirmation).
- Parent-side gate: `git -C <wt> status --short` in the worktree and in the main checkout.

## Traps

- **Probing against the user's live daemon.** Running the native client (or any test daemon) with default dirs attaches to the user's real sessions: the client's `Resize` changes their PTYs, and a probe daemon's startup reap kills every `rt-tracer*` under its binaries dir that its own config dir doesn't reference, so one started with only `RUSTLING_TULIP_CONFIG_DIR` isolated kills the installed app's live sessions machine-wide. A spawned daemon sets all of `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR` and `RUSTLING_TULIP_WORKTREES_DIR` to `.tmp/` dirs (as `tools/e2e/wdio.conf.ts` does); a client run against the real daemon attaches only to a throwaway plain-shell session the brief names.
- **GPUI in the workspace makes cold builds heavy.** Once `apps/native` is a member, `cargo clippy --workspace` and `cargo test --workspace` compile GPUI; a fresh worktree's first build does it from scratch. Each worktree keeps its own default `target/` (13–37 GB): a shared `CARGO_TARGET_DIR` mixed up two trees' `protocol` builds (user, 2026-09-25). The in-tree `target/` goes when the landing step removes the worktree.
- **A fresh worktree can't pass workspace clippy (or the prek clippy hook) until the Tauri build inputs exist.** `tauri-build` fails with `resource path binaries\rustling-tulipd-x86_64-pc-windows-msvc.exe doesn't exist` until the daemon and tracer are staged as sidecars, and it also needs an `apps/tauri-app/dist/` directory. Before the first gate in a new worktree, run `.\rt.ps1 build` there (it builds `daemon` + `tracer` and stages them into `apps/tauri-app/src-tauri/binaries/`) and `New-Item -ItemType Directory -Force apps/tauri-app/dist`. Both paths are gitignored, so nothing lands in the diff. Every brief carries this as its setup step. `rc.exe` was not needed on this machine (not on PATH; clippy passed in five worktrees, 2026-09-25).
- **No `git stash`** to isolate edits, since parallel agents share the tree: show an error is pre-existing with `git diff -- <file>` instead.
- **Stale sidecar binaries:** `.\rt.ps1 installer` copies only when the target is older than the source; delete a suspicious binary and rebuild rather than trusting "Finished".
- **E2E (Tauri bug fixes only):** run one spec with `pnpm exec wdio run wdio.conf.ts --spec <file>` from `tools/e2e`; a full parallel run flakes about one spec per run, so rerun a failure alone; `pnpm run doctor` (not `pnpm doctor`) checks msedgedriver against WebView2.

## Concurrency

- Worktrees: `git worktree add ../rustling-tulip.wt/<slug> -b <slug> main` from the main checkout.
- Ceiling: **two** implementers while Phase 1 is current, because P1.4–P1.8 all mount into the same root view and app state, and most pairs wait on each other anyway. Raise it to three from Phase 2, whose items separate by subsystem.
- Depends on, where file lists hide it: P1.2 → P1.3 (the crate the connection code calls); P1.1 → every native item; a protocol message one item adds and another item's view consumes.
- Context: read the status bar's figure at every landing (`jq .context_window.used_percentage <scratchpad>/statusline.json`); past 40% the loop stops refilling, per the user-level `nextup`.

## Docs map

| What changed | Owning docs |
|---|---|
| Native feature delivered | tick the item in `docs/plans/native-client.md` and the parity lines it covered in `docs/plans/native-client-parity.md` |
| A crate, binary, `rt.ps1` verb, env var or on-disk path added, moved or removed | `CLAUDE.md` (Project shape, Common commands, Environment variables, Where things live), `README.md` Layout / Build |
| Wire protocol | `crates/protocol/src/lib.rs` doc comments, the TS mirror (until cutover), CLAUDE.md "Wire-protocol gotchas" when a rule changes |
| Tracer ABI | `docs/tracer-abi.md` |
| A term used in a project-specific sense (a new plan word, a phase name) | `README.md` Glossary |
| A plan finished | `git mv` to `docs/plans/completed/`, one-line entry under "Shipped" in `docs/plan.md` |

The repo keeps no CHANGELOG.

## Landing

- Commit in the worktree (fast-forward it onto `main` first if `main` moved; rerun the gates if that brought in new commits). Commit messages: ≤4-char type tag, imperative, ≤72-char subject, the session's attribution trailers.
- From the main checkout: `git merge --ff-only <slug>`, then `git push origin main`. Pushing straight to `main` is the user's standing rule for this solo repo.
- Then `git merge-base --is-ancestor <slug> main` && `git worktree remove ../rustling-tulip.wt/<slug>` && `git branch -d <slug>`.
- The main checkout hosts at most one implementer, and none while a gate runs there.
