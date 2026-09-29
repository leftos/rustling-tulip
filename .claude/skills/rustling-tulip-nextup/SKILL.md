---
name: rustling-tulip-nextup
description: Profile for the user-level `nextup` skill in the rustling-tulip repo — loaded by `nextup` at its step 0 for this project's plan convention, agents, gates, docs map and landing path. Not a loop of its own; invoke `/nextup`.
---

# rustling-tulip profile for `nextup`

The generic loop is the user-level `nextup` skill; this file supplies only what is rustling-tulip-specific.

## Plan and tracker

- Index: `docs/plans/MAIN.md`. Order: High priority, then the waves top to bottom (the first wave is the one in flight), then Backlog and singles; the next item is the first line from the top. Each line links the subplan holding its rulings (native client items: `docs/plans/native-client.md`; feature scope: the parity sections it names in `docs/plans/native-client-parity.md`). Each wave names its shared files, review and verification split.
- A coarse line (Phase 5, Phase 6, the mobile app's phases, a design that still needs a pass) is a *design* item when its wave comes up: split it into brief-sized items (`P<n>.<m>` for the native client), put the split to the user in the decision round before anything is dispatched, and replace the coarse line with one line per item.
- Siblings: none.
- Pre-loop hooks: none.
- Finished-item convention: **delete the line** from `MAIN.md` in the landing commit (`git log` is the record), and delete or tick its entry in the subplan it links. Tick the parity lines the item delivered in `native-client-parity.md` in the same commit. The commit that lands a wave's last line deletes the wave's heading and renumbers the rest. A review finding the item doesn't fix becomes a new line in the wave sharing its files. When a subplan's last item lands, promote its durable decisions into `docs/` (`architecture.md`, `native-client.md`, or a doc of their own) and delete the subplan in that commit; nothing new moves to `docs/plans/completed/`.
- Tracker: `gh issue list --repo leftos/rustling-tulip --state open --json number,title`. No triage skill; place issues by the step-0 rule. A bug in the Tauri app is fixed on the `tauri` branch, not `main`; a daemon fix lands on `main` first and is cherry-picked there.
- Pull requests: `gh pr list --repo leftos/rustling-tulip --state open --json number,title,baseRefName`. An unplanned PR gets a line by the step-0 rule, naming its base branch (`main` or `tauri`), its files, whether the checks pass and whether it merges cleanly; a bot's dependency bump goes to Backlog.
- Hotspots (two items touching one wait on each other): `crates/protocol/src/lib.rs`, `crates/daemon/src/server.rs`, and in the native client `apps/native/src/lib.rs` (`RootView`), which every native item mounts into.

## Rulings every brief carries

- **Protocol:** additive changes only, the `add-protocol-message` skill's 3-step pattern, no `protocol-version.json` bump for additive changes (CLAUDE.md "When to bump").
- **Protocol 22 stays decodable** while the installed Tauri app is in use: a protocol change keeps every v22 message decoding, proved by `cargo test -p protocol v22_compat`.
- **Testable core, thin view:** state that a feature adds (sidebar tree, split tree, tab list, key mapping, span building) lives in plain-Rust modules with unit tests; the GPUI view only renders it and forwards events. The proving command is that module's `cargo test -p <crate> <filter>`, red first.

## Agents and gates

- Explore: `Explore`. Rust design second opinion: `oracle`.
- Reviewers: `code-review` for every item.
- Gates, each run through the repo's gate from the worktree root as `pwsh tools/gate.ps1 -Log .tmp/<name>.log -TimeoutSeconds <n> -Slot <heavy|light> -- <command>` (no `nice` of its own; the gate lowers priority, takes a machine-wide slot, logs the whole output and prints the tail):
  - `cargo fmt --all --check` (light)
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` (heavy; what `.\rt.ps1 clippy` and prek run)
  - `cargo test -p <crate>` for each touched crate (heavy; for `crates/protocol` this includes `v22_compat`)
  - `cargo deny check` when `Cargo.toml` or `Cargo.lock` changed (light)
- UI that only a person can verify: when a native item's visible result can't be proved by a test, it lands on green gates, and the checkpoint lists it under "hand-test" with what to look at. A **visual fix** (something already landed that looks wrong) is not committed until the user confirms it by hand-testing; edit, ask, then squash (memory: commit-only-after-confirmation).
- Parent-side gate: `git -C <wt> status --short` in the worktree and in the main checkout.

## Traps

- **Probing against the user's live daemon.** Running the native client (or any test daemon) with default dirs attaches to the user's real sessions: the client's `Resize` changes their PTYs, and a probe daemon's startup reap kills every `rt-tracer*` under its binaries dir that its own config dir doesn't reference, so one started with only `RUSTLING_TULIP_CONFIG_DIR` isolated kills the installed app's live sessions machine-wide. A spawned daemon sets all of `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR` and `RUSTLING_TULIP_WORKTREES_DIR` to `.tmp/` dirs (as `rt.ps1 native-e2e` does); a client run against the real daemon attaches only to a throwaway plain-shell session the brief names.
- **GPUI in the workspace makes cold builds heavy.** Once `apps/native` is a member, `cargo clippy --workspace` and `cargo test --workspace` compile GPUI; a fresh worktree's first build does it from scratch. Each worktree keeps its own default `target/` (13–37 GB): a shared `CARGO_TARGET_DIR` mixed up two trees' `protocol` builds. The in-tree `target/` goes when the landing step removes the worktree.
- **No `git stash`** to isolate edits, since parallel agents share the tree: show an error is pre-existing with `git diff -- <file>` instead.

## Concurrency

- Worktrees: `git worktree add ../rustling-tulip.wt/<slug> -b <slug> <base>` from the main checkout, then `branch.<slug>.base` and `branch.<slug>.landOn` recorded as the user-level `nextup` §3 **Base and target** says (`main` and `main` by default; a Tauri fix is cut from and lands on `tauri`).
- Ceiling: **three** implementers; items inside one wave share files, so parallel items come from different waves or from a wave whose subplan says its steps separate.
- Depends on, where file lists hide it: the dependencies a line or its subplan names (P4.4b → P4.12c, P4.12b → P4.12c, P4.11 → P4.15b, Wave 6 → Wave 7); a protocol message one item adds and another item's view consumes.
- Context: read the status bar's figure at every landing (`jq .context_window.used_percentage <scratchpad>/statusline.json`); past 40% the loop stops refilling, per the user-level `nextup`.

## Docs map

| What changed | Owning docs |
|---|---|
| Native feature delivered | delete its `docs/plans/MAIN.md` line and its section in `docs/plans/native-client.md`, tick the parity lines it covered in `docs/plans/native-client-parity.md`, and record any lasting design decision in `docs/native-client.md` |
| A file, module or flow a task-index row names added, moved or removed | `docs/architecture.md` (Components, Task index) |
| A crate, binary, `rt.ps1` verb, env var or on-disk path added, moved or removed | `CLAUDE.md` (Project shape, Common commands, Environment variables, Where things live), `README.md` Layout / Build |
| Wire protocol | `crates/protocol/src/lib.rs` doc comments, CLAUDE.md "Wire-protocol gotchas" when a rule changes |
| Tracer ABI | `docs/tracer-abi.md` |
| A term used in a project-specific sense (a new plan word, a phase name) | `README.md` Glossary |
| A subplan finished | durable decisions promoted into `docs/` (and the feature summary in `docs/architecture.md` "What it does"), then the subplan deleted and its `MAIN.md` link removed |
The repo keeps no CHANGELOG.

## Landing

- Commit in the worktree (fast-forward it onto its `landOn` branch first if that moved; rerun the gates if that brought in new commits). Commit messages: ≤4-char type tag, imperative, ≤72-char subject, the session's attribution trailers. A stacked item lands after the item under it.
- From the checkout that has `landOn` out (the main checkout for `main`): `git merge --ff-only <slug>`, then `git push origin <landOn>` when `origin/<landOn>` exists (a local session branch is not pushed; it ships with its own branch). Pushing straight to `main` is the user's standing rule for this solo repo. A session running from a worktree offers `/ship` at each push point instead of pushing (user-level `nextup`, "A worktree session offers a ship instead of a push").
- Then, once `git cherry <landOn> <slug> <base sha>` prints no `+` line (user-level `nextup` §4 step 6): `git worktree remove ../rustling-tulip.wt/<slug>` && `git branch -D <slug>`.
- The main checkout hosts at most one implementer, and none while a gate runs there.
