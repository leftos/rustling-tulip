# Agent skill pack

Design for the "Agent skill pack" line in [MAIN.md](./MAIN.md) ("Ideas needing a design pass"), steps `SP.n`. It takes the place of the plan-then-execute feature dropped from [agent-cli.md](./agent-cli.md) (its former part 2, steps PE.1–PE.6, and Open questions 12–14) and [borrowed-ideas.md](./borrowed-ideas.md) ("Plan, then execute in parallel (dropped)"), and it absorbs the skill half of AC.10 (agent-cli.md Open question 11, answered (a): a skill shipped in the repo that the user installs once, plus `agent help`).

## Problem

A rustling-tulip user often has one task that splits into parts: three independent fixes, a feature whose daemon, protocol and client halves can be built side by side, a refactor across several crates. What they want is for the agent in front of them to split the task, run each part in its own worktree at the same time, check each result, send short-falling work back, and merge what passes into its own branch, with the user asked only where a decision is theirs.

Claude Code already has the pieces: the `Agent` tool starts subagents, `git worktree add` gives each its own tree, and the agent that split the task holds the context to judge the results. What it lacks is the protocol that makes this reliable: how to size a part, what a brief must contain, how a worker proves its edits landed in its own tree and not the parent's, what a report looks like, how the parent checks the tree itself instead of trusting the report, when a fix round goes back to the same worker, and how to merge and clean up. This machine runs that protocol through its own skills (`plan-execution`, `parallel-worktree-agents`, `nextup`) and its `implementer` agent; the pack is a general version of it that any user can install.

Why a skill pack rather than a built-in feature: the dropped design put the split in a daemon-side plan record, the approval in a review dialog, and the part state machine and merge policy in Rust. Every one of those steps is a judgement (is this split independent, is this diff right, does this conflict resolve this way) that an agent makes better than a state machine, and a skill changes by editing a Markdown file where a built-in feature needs a protocol message, a daemon release and a client release. What rustling-tulip adds is the part only it can do: the agent CLI (agent-cli.md) makes a worker a real session the user can watch, type into and approve, instead of a subagent hidden inside the parent's transcript.

## Rulings

- **Ruling (user): no built-in plan-then-execute.** Splitting, reviewing and merging are the agent's job; rustling-tulip provides sessions, worktrees and the agent CLI.
- **Ruling (agent-cli.md Q11 (a)): the agent CLI's skill ships in the repo and the user installs it once**; the pack is where it ships.
- **Claude Code first.** Skills and agent files are Claude Code formats. Codex and Cursor sessions still get the agent CLI and `agent help`; a pack for their formats is a later item, not this one.
- **Names carry an `rt-` prefix** (`rt-fan-out`, `rt-review-merge`, `rt-sessions`, agent `rt-worker`), so an installed pack never shadows a user's own `implementer` or `plan-execution`.
- **No personal specifics.** This machine's gate script, memory server, DeepSeek dispatch, tier bands, cost figures and paths stay out. The pack reads a project's build and test commands from the project's own `CLAUDE.md`, `AGENTS.md` or README, and asks the user once when none names them.
- **Merging is the parent agent's** (the user's ruling above). Conflict handling is Open question 3.

## Design

### What ships

The pack is a folder in the repo, `agent-pack/`, holding three skills and one agent definition. Each file is plain Markdown with front matter, readable without rustling-tulip.

| File | Trigger (front-matter description, paraphrased) | What it tells the agent to do |
| - | - | - |
| `agent-pack/skills/rt-fan-out/SKILL.md` | The user asks to split a task, run parts in parallel, fan out, or "do these in worktrees"; or the agent finds a task has two or more independent parts each worth a build-and-test loop | Split, size, conflict-scan, brief, dispatch, keep a ledger (below) |
| `agent-pack/skills/rt-review-merge/SKILL.md` | A worker reported; or the user asks to review or merge worker branches | Parent-side gate, review, fix rounds, merge, clean up (below) |
| `agent-pack/skills/rt-sessions/SKILL.md` | `RT_CLI` is set in the environment, or the user mentions rustling-tulip sessions, spawning or messaging another session | The agent CLI: verbs, approval, limits, reading a report, waiting (below); this is AC.10's skill |
| `agent-pack/agents/rt-worker.md` | Dispatched by `rt-fan-out` only | Execute one brief in one worktree and report in a fixed format (below) |

### `rt-fan-out`: splitting and dispatching

1. **Decide whether to fan out at all.** Parts are independent when their file sets are disjoint and neither consumes a type, API or format the other changes. Related failures (one change turning several tests red) go to one worker, not several. A task with one part, or parts too small to repay a worker's startup (a one-file edit whose test is expected to pass), is done inline.
2. **Size each brief** for roughly four steps or six files; a larger part is split, or run as consecutive briefs to the same worker.
3. **Conflict scan, written out.** Before dispatching two briefs at once, write one row per pair that shares a file or an interface, with its resolution: run them one after the other, merge the briefs, or split the shared file out. A scan without rows is not a scan.
4. **Ask before starting** when the split has more than three parts or touches files the user did not name: one question listing the parts, each with its files. The user can drop or merge parts. This is the approval the dropped review dialog gave, held in the conversation.
5. **One worktree per worker**, made by the parent with `git worktree add <root>/<part-slug> -b <branch>-<part-slug> HEAD`, where `<root>` is a folder beside the parent's own tree (`<parent tree>/../<repo>.fan`). The `Agent` tool's own `isolation: "worktree"` option is not relied on: on this machine it created the tree without scoping the subagent's edits to it (the `parallel-worktree-agents` skill, "does NOT reliably scope file edits"). A workspace session makes one tree per member under the same `<part-slug>` folder, laid out like the members so their relative paths resolve.
6. **The brief** carries the absolute worktree root, the files to touch, the change with no design decision left open, and a proving command that fails if the change is wrong (a new behaviour names a new test). A behaviour of a library or tool the brief relies on is quoted from its docs or source, not from memory.
7. **Dispatch** every independent brief in one message, as `rt-worker` subagents, up to a ceiling of three running at once (Open question 2 sets when a worker is a session instead).
8. **The ledger**: one line per event (`dispatched`, `complete`, `fix round N`, each ruling a worker reports) in `$(git rev-parse --git-common-dir)/rt-fan-out/ledger.md`, which every worktree of the repo shares and git never tracks. After a compaction or `/clear`, the parent reads the ledger before re-dispatching anything; a part with a `complete` line is never sent again.

### `rt-worker`: one brief, one tree

- **First three commands** before any edit: `pwd`, `git rev-parse --show-toplevel` (kept as the tree root), and a listing of a folder the brief names. Every Read, Edit and Write uses an absolute path under that root; after each turn of edits, `git -C <root> status --short <files>` must show each file, or the worker stops and reports. The prompt names the trap it guards against: a worktree path that ends in the same segments as the main checkout invites rewriting it to the main checkout.
- **No `cd` prefixes**, no edits outside the named files, no commits, no docs, no spawning further agents.
- **Loop per step**: format, build, the proving command, at most two fixes of its own edit on red, then stop and report. Never relax an assertion or delete a test to go green. Command output goes to a log file with only the tail printed.
- **A gap in the brief** is ruled on when a wrong answer is cheap and visible in the diff (a default, a name, a wording), and reported as `Ruling: what — why — cost if wrong`; it stops the worker as `underspecified` when the choice is destructive, security-sensitive, leaves the tree, or changes another step.
- **Budget**: at about 100 tool calls, finish the step in flight and report `partial`.
- **Report**, every line filled: `STATUS` (done, partial, blocked, underspecified), `TREE`, `FILES` (`git status --short` verbatim), `TESTS` (per step, command and summary line), `RULINGS`, `BLOCKER`, `REMAINING`, `CALLS`, `OBSERVATIONS`, `SURFACES` (what a user or developer now sees differently). Kept under 8 KiB so a session worker's report fits `agent read --last` (agent-cli.md Q8).
- **Model and tools**: no `tools:` list, so the worker inherits the parent's tools and MCP servers; `model: inherit`. Open question 4.

### `rt-review-merge`: review, fix rounds, merge

1. **The report is not evidence.** Run `git -C <worker tree> status --short` and `git -C <parent tree> status --short`; a stray edit in the parent's tree is reverted before anything else.
2. **Read `RULINGS` first**, check each against the brief, reverse a wrong one now. Then read the diff sentence by sentence against the brief's constraints: a green test proves only what the test encodes.
3. **Fix rounds** go back to the same worker (`SendMessage` to a subagent, `agent send` to a session) for rounds 1 to 3; round 4 goes to a fresh worker against the same tree with the findings in its brief; round 5 stops and puts each open finding to the user.
4. **Merge** each accepted part into the parent's branch, one at a time, in dependency order: the worker commits nothing, so the parent commits in the worker's tree (one commit per part, subject naming the part), then `git merge --no-ff <part branch>` in its own tree, then runs the project's tests before the next merge. Conflicts: Open question 3.
5. **Clean up** once a part is merged: `git merge-base --is-ancestor <part branch> HEAD`, then `git worktree remove <tree>` and `git branch -d <part branch>`. A session worker's tree belongs to its session, so it is left for the user (Open question 5).
6. **Close** with one line per part (merged sha, or why it was dropped), the branches and trees left, and anything a person must test by hand.

### `rt-sessions`: workers as rustling-tulip sessions

With the agent CLI (agent-cli.md, AC.1–AC.9) present, `rt-fan-out` can make a worker a session instead of a subagent: `& $env:RT_CLI agent spawn --worktree --branch <branch>-<part-slug> --label <part> --file <brief>` creates the tree through the normal spawn path (rustling-tulip's worktrees root, its branch-name checks, the `↳` tag, the user's approval card), so step 5's `git worktree add` is skipped. The brief then opens with the `rt-worker` rules, since a session is not started as a subagent type.

- **Waiting and reading**: `agent wait <ids…>` (one or all idle, ended or awaiting, 600 s a call, called again until done); `agent read <id> --last` for the report; `agent read <id> --transcript 20` when the report is missing.
- **An `awaiting` worker** is waiting on a permission prompt or a question: the parent tells the user which session and why, and never answers a permission prompt through `send`.
- **Fix rounds** are `agent send <id> --file <findings>`; the worker keeps its context.
- **Limits and refusals** (agent-cli.md Q5, Q6): a denied spawn (exit 4) is reported to the user, never retried; a refused one (exit 3, a limit) waits for a running worker to finish.
- **When a session beats a subagent**: the user wants to watch or steer a part, the part runs long enough to outlive the parent's context, or it needs its own permission answers. Open question 2 sets the default.

Without the agent CLI (Codex, Cursor, a Claude session outside rustling-tulip, or before AC lands), `rt-sessions` is not triggered and `rt-fan-out` uses subagents only. The pack is useful on day one; AC adds the session path.

### Installing

Recommended (Open question 1): the native client's Settings gains an "Agent skills" row with Install and Update. The pack is embedded in the client with `include_str!`; Install writes `~/.claude/skills/rt-*/SKILL.md` and `~/.claude/agents/rt-worker.md` and a stamp file, `~/.claude/skills/.rt-agent-pack.json`, holding the pack version and each file's hash. Update rewrites only files whose hash still matches the stamp; a file the user edited is left, and the row names it. The row shows "Installed", "Update available" or "Not installed", read from the stamp. The dev path is `.\rt.ps1 agent-pack install`, which calls the same code through the client binary's `--install-agent-pack` flag.

## Steps

Order: SP.0 → SP.1 → SP.2, SP.3 and SP.4 in any order → SP.5 → SP.6; SP.7 needs AC.7 and SP.5; SP.8 follows the rest. The gates are the nextup profile's (`.claude/skills/rustling-tulip-nextup/SKILL.md`, "Agents and gates"); a skill step's proof is its eval scenario plus the format check.

- [ ] **SP.0 Spike: verify the Claude Code behaviour the pack leans on.** With a real `claude` on Windows: a skill in `~/.claude/skills/<name>/SKILL.md` and an agent in `~/.claude/agents/` are found in a new session; a subagent started as `rt-worker` with no `tools:` list gets the parent's MCP tools; `model: inherit` is honoured; `SendMessage` reaches a finished subagent; a `git worktree add` beside a rustling-tulip session's tree shows in Manage worktrees as that group's member, and discarding the session leaves it alone; whether `isolation: "worktree"` now scopes edits (if it does, step 5 may use it). Record in `docs/spikes/agent-skill-pack.md`. Proof: the note, each answer quoted from a run.
- [ ] **SP.1 Pack skeleton and format check.** New `agent-pack/` with the four files as stubs; new `apps/native/src/agent_pack.rs` embedding them (`include_str!`) with a `FILES` table; unit tests that parse each front matter (`name` equals the folder, `description` present and at most 1024 characters), that every relative link resolves inside the pack, and that no file names a personal specific (`gate.ps1`, `mem0`, `DeepSeek`, `C:/Users`, `leftos`). Proof: `cargo test -p rustling-tulip-native agent_pack`, red first on a stub with a mismatched `name`.
- [ ] **SP.2 `rt-fan-out`.** `agent-pack/skills/rt-fan-out/SKILL.md` as designed above. Proof: eval scenarios E1 (two independent parts: two trees, two `rt-worker` dispatches in one message, a ledger with two `dispatched` lines) and E2 (a one-file task: no fan-out) pass through SP.5's runner; SP.1's check green.
- [ ] **SP.3 `rt-worker`.** `agent-pack/agents/rt-worker.md` as designed above. Proof: eval E3 (a brief whose tree path ends in the main checkout's segments: every edit lands in the worker's tree, the main checkout stays clean) and E4 (a brief missing its proving command: `STATUS: underspecified`, no edits); SP.1's check green.
- [ ] **SP.4 `rt-review-merge`.** `agent-pack/skills/rt-review-merge/SKILL.md` as designed above. Proof: eval E5 (one part with a planted wrong ruling: reversed before merge; one part merged; both trees and branches removed) and E6 (two parts that conflict: behaviour per Open question 3); SP.1's check green.
- [ ] **SP.5 Eval runner.** New `tools/agent-pack-evals/`: a fixture repo generator (a small Rust crate with two independent functions, their tests and a shared file for the conflict case), one folder per scenario holding the prompt and a `check.ps1` that asserts on git state (trees, branches, merges, `git status` of the main checkout, the ledger); a `run.ps1` that installs the pack into a throwaway `CLAUDE_CONFIG_DIR`, runs `claude -p` per scenario and runs its check. Opt-in and paid, never in the gates; `.\rt.ps1 agent-pack eval [-Scenario E1]`. Proof: a run of E1–E6 with each check's output recorded in `docs/spikes/agent-skill-pack.md`, and each check shown red against a hand-broken outcome.
- [ ] **SP.6 Install.** Per Open question 1: `agent_pack.rs` gains `install(home) -> Report` and `status(home)` (stamp read and write, hash-guarded update, edited files kept and named); the Settings row in the General tab; `--install-agent-pack` in `main.rs`; `rt.ps1`'s `agent-pack install` verb and its help line. Proof: unit tests over a temp home: fresh install, re-install is a no-op, update rewrites an untouched file, an edited file is kept and named, a missing `~/.claude` is created; `cargo test -p rustling-tulip-native agent_pack`; hand-test the row.
- [ ] **SP.7 `rt-sessions` (AC.10's skill).** `agent-pack/skills/rt-sessions/SKILL.md` as designed above, checked against `agent help`'s text; `rt-fan-out` and `rt-review-merge` gain their session branches. Proof: eval E7 in SP.5's runner against an isolated daemon (the three `RUSTLING_TULIP_*_DIR` variables under `.tmp/`, as `rt.ps1 native-e2e` sets them): the parent spawns two session workers, the test approves both through the daemon, the parent reads both reports with `agent read --last` and merges; SP.1's check green.
- [ ] **SP.8 Docs.** `docs/architecture.md` ("What it does": an "Agent skill pack" line; task-index row "Change the agent skill pack": `agent-pack/`, `apps/native/src/agent_pack.rs`, `tools/agent-pack-evals/`); CLAUDE.md (Project shape: `agent-pack/`; Common commands: `rt.ps1 agent-pack`); `docs/native-client.md` (the Settings row); README glossary (below, and `SP.1` in the step-id entry); agent-cli.md's AC.10 loses its skill half; delete this doc once promoted. Proof: the diff.

## Open questions

Answered (user): Q1 (a) a Settings row in the native client (Install / Update) writing the embedded files into `~/.claude/skills` and `~/.claude/agents` with a hash stamp, plus `rt.ps1 agent-pack install`; Q2 (a) workers are subagents by default, a session when the user asks for one or a part is expected to run long or needs its own permission answers, named in `rt-fan-out`'s split question; Q3 (a) the parent resolves a mechanical conflict between its own workers' output, reruns the tests and records it in the ledger, and asks the user when the conflict touches unbriefed code or needs a choice between behaviours; Q4 (a) `rt-worker.md` ships with no `tools:` list and `model: inherit`; Q5 (a) the user removes session workers: the parent's closing lines name each finished worker session, and Discard reaps its merged branch.
