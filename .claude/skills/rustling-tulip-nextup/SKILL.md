---
name: rustling-tulip-nextup
description: Profile for the user-level `nextup` skill in the rustling-tulip repo — loaded by `nextup` at its step 0 for this project's plan convention, agents, gates, docs map and landing path. Not a loop of its own; invoke `/nextup`.
---

# rustling-tulip profile for `nextup`

The generic loop is the user-level `nextup` skill; this file supplies only what is rustling-tulip-specific.

siblings: none
linear: rustling-tulip

## Plan and tracker

- The plan lives in Linear: every task is a Linear issue in team RT, per `~/.claude/docs/plan-operations.md`; `docs/plans/MAIN.md` is its generated snapshot, never edited by hand. Project order, which is the order the queue is worked: the waves, Wave 1a to Wave 12 (the first is the one in flight), then `Docs`, `Build tooling`, `macOS`, `Agent CLIs`, `Agent harnesses`, `Open questions and blocked items`, `Ideas needing a design pass` and `Backlog`. A bug report or anything that blocks a wave goes in a `High priority` project ahead of the waves, which it outranks. Each wave's project content names its shared files, review gate, verification split and design file. A new item joins the wave whose files or subject it shares, or opens a new wave project after the last wave.
- An issue is one item: its title the action, its description the files, who asked and a link to its design file in `docs/plans/` (native client items: `docs/plans/native-client.md`; feature scope: the parity sections in `docs/plans/native-client-parity.md`). Decisions and rulings a brief needs live in that design file or in `docs/`, never in the description. A native feature ticks the parity lines it delivered in `native-client-parity.md` in its own commit.
- A coarse issue (Phase 5, Phase 6, the mobile app's phases, a design that still needs a pass) is a *design* item when its wave comes up: split it into brief-sized items (`P<n>.<m>` for the native client), put the split to the user in the decision round before anything is dispatched, and give each part an **add** with `--parent` the coarse issue.
- A review finding the item doesn't fix gets an **add** in the wave sharing its files.
- Subplan fate: a design file stays in `docs/plans/` while an open issue or project links it. Once nothing open links it, its durable decisions are promoted into `docs/` (`architecture.md`, `native-client.md`, or a doc of their own) and the design file is deleted in that commit, so under `plan-doc-hygiene` its verdict is promote, then delete. `docs/plans/archive/` holds designs finished before the plan moved to Linear, left as they are; nothing new moves there.
- Feature markers: a wave on a feature branch carries `branch: feat/<name>` in its project's content. The first item under it opens the branch and its draft feature PR, and the project gets a tracking issue `Merge feat/<name> (#N)`, last in the project. An item that lands on the branch is **land**ed with the note `on feat/<name>, ships with #N`.
- Pre-loop hooks: none.
- Tracker: **triage** as plan-operations says (GitHub issues reach team RT through Linear's sync; an untriaged one is top-level with no project), each placed in the project that shares its files, else in `Backlog`. A bug in the Tauri app is fixed on the `tauri` branch, not `main`; a daemon fix lands on `main` first and is cherry-picked there.
- Pull requests: `gh pr list --repo leftos/rustling-tulip --state open --json number,title,baseRefName`. An unplanned PR gets an **add**, "Review and land PR #N", naming its base branch (`main` or `tauri`), its files, whether the checks pass and whether it merges cleanly; a bot's dependency bump goes to `Backlog`.
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
  - the live tier, `pwsh ./rt.ps1 native-e2e` (heavy), and the OS tier, `pwsh ./rt.ps1 native-smoke` (light), when a wave's verification names them
- UI that only a person can verify: when a native item's visible result can't be proved by a test, it lands on green gates, and the checkpoint lists it under "hand-test" with what to look at. A **visual fix** (something already landed that looks wrong) is not committed until the user confirms it by hand-testing; edit, ask, then squash (memory: commit-only-after-confirmation).
- Parent-side gate: `git -C <wt> status --short` in the worktree and in the main checkout.

## Traps

- **Probing against the user's live daemon.** Running the native client (or any test daemon) with default dirs attaches to the user's real sessions: the client's `Resize` changes their PTYs, and a probe daemon's startup reap kills every `rt-tracer*` under its binaries dir that its own config dir doesn't reference, so one started with only `RUSTLING_TULIP_CONFIG_DIR` isolated kills the installed app's live sessions machine-wide. A spawned daemon sets all of `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR` and `RUSTLING_TULIP_WORKTREES_DIR` to `.tmp/` dirs (as `rt.ps1 native-e2e` does); a client run against the real daemon attaches only to a throwaway plain-shell session the brief names.
- **GPUI in the workspace makes cold builds heavy.** Once `apps/native` is a member, `cargo clippy --workspace` and `cargo test --workspace` compile GPUI; a fresh worktree's first build does it from scratch. Each worktree keeps its own default `target/` (13–37 GB): a shared `CARGO_TARGET_DIR` mixed up two trees' `protocol` builds. The in-tree `target/` goes when the landing step removes the worktree.
- **No `git stash`** to isolate edits, since parallel agents share the tree: show an error is pre-existing with `git diff -- <file>` instead.

## Concurrency

- Worktrees: `git worktree add ../rustling-tulip.wt/<slug> -b <slug> <base>` from the main checkout, then `branch.<slug>.base` and `branch.<slug>.landOn` recorded as the user-level `nextup` §3 **Base and target** says (`main` and `main` by default; a Tauri fix is cut from and lands on `tauri`).
- Ceiling: **three** implementers; items inside one wave share files, so parallel items come from different waves or from a wave whose subplan says its steps separate.
- Depends on, where file lists hide it: the dependencies an issue or its design file names (P4.4b → P4.12c, P4.12b → P4.12c, P4.11 → P4.15b, Wave 5 → Wave 6); a protocol message one item adds and another item's view consumes.
- Context: read the status bar's figure at every landing (`jq .context_window.used_percentage <scratchpad>/statusline.json`); past 40% the loop stops refilling, per the user-level `nextup`.

## Docs map

| What changed | Owning docs |
|---|---|
| Native feature delivered | **land** its issue, delete its section in `docs/plans/native-client.md`, tick the parity lines it covered in `docs/plans/native-client-parity.md`, and record any lasting design decision in `docs/native-client.md` |
| A file, module or flow a task-index row names added, moved or removed | `docs/architecture.md` (Components, Task index) |
| A crate, binary, `rt.ps1` verb, env var or on-disk path added, moved or removed | `CLAUDE.md` (Project shape, Common commands, Environment variables, Where things live), `README.md` Layout / Build |
| Wire protocol | `crates/protocol/src/lib.rs` doc comments, CLAUDE.md "Wire-protocol gotchas" when a rule changes |
| Tracer ABI | `docs/tracer-abi.md` |
| A term used in a project-specific sense (a new plan word, a phase name) | `README.md` Glossary |
| A subplan finished | durable decisions promoted into `docs/` (and the feature summary in `docs/architecture.md` "What it does"), then the design file deleted |

## Changelog

The repo keeps no CHANGELOG. Read by the user-level `changelog-and-commit`:

- Plan (Step 2b): **land** each item after its commit.

## Landing

- Commit in the worktree (fast-forward it onto its `landOn` branch first if that moved; rerun the gates if that brought in new commits). Commit messages: ≤4-char type tag, imperative, ≤72-char subject, the session's attribution trailers. A stacked item lands after the item under it.
- From the checkout that has `landOn` out (the main checkout for `main`): `git merge --ff-only <slug>`, then `git push origin <landOn>` when `origin/<landOn>` exists (a local session branch is not pushed; it ships with its own branch). Pushing straight to `main` is the user's standing rule for this solo repo. A session running from a worktree offers `/ship` at each push point instead of pushing (user-level `nextup`, "A worktree session offers a ship instead of a push").
- An item under a feature marker (user-level `nextup` §3, "Feature branches") has `landOn` = `feat/<name>`: the `--ff-only` merge runs in the feature worktree (`../rustling-tulip.wt/feat-<name>`) and `git push origin feat/<name>` follows; the item is **land**ed with the note `on feat/<name>, ships with #N`, and the feature PR into `main` merges only through `/ship` on the feature branch. The repo has no CI, so the gates above stand in for the PR's checks.
- Then, once `git cherry <landOn> <slug> <base sha>` prints no `+` line (user-level `nextup` §4 step 6): `git worktree remove ../rustling-tulip.wt/<slug>` && `git branch -D <slug>`.
- The main checkout hosts at most one implementer, and none while a gate runs there.
