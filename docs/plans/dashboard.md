# Dashboard

Design for RT-28 (Wave 9): one view of every live agent session, so the user learns what each one is doing, what it has done and what it waits on without opening its terminal. It absorbs ops-tower's Dashboard and Agents views (`X:\dev\ops-tower`, `docs/dashboard.md` and `docs/agents-transcripts.md`) and is fed by a summarizer service in the daemon.

Mocks (approved): [canvases/dashboard/index.html](./canvases/dashboard/index.html), lane A ("Dense rows"), the By status view; [canvases/ops-tower-absorb/ot-dashboard-additions/index.html](./canvases/ops-tower-absorb/ot-dashboard-additions/index.html), lane B ("By repo lane, with Next up"), the default view.

Their screenshots rebuild from [canvases/dashboard/RECIPE.md](./canvases/dashboard/RECIPE.md) and [canvases/ops-tower-absorb/RECIPE.md](./canvases/ops-tower-absorb/RECIPE.md). Evidence: [spikes/waiting-alerts.md](../spikes/waiting-alerts.md) (the summarizer prompt), [spikes/local-summarizer.md](../spikes/local-summarizer.md) (RT-27, the provider). Status source: [hook-status.md](./hook-status.md).

## Goal

With many sessions open, the user has no single place that answers "what is everything doing, who is waiting on me, and what got done". Today that means reading each terminal, the Needs You panel and Linear side by side. The Dashboard puts one dense row per session in a tab: its issue, what it runs this second, a running summary, its subagents and background work, its context, activity and cost, and its waiting question answered in place.

## Rulings (owner)

- **Layout**: the Dashboard is lane A of the mock, dense rows: one two-line row per session (session, Linear, what it runs now, summary, agents, context, 10-minute activity, runtime and last activity, cost), questions and permissions answered inline, subagents, gates and background shells nested under a working session, and a drawer per row with the running summary timeline, recent work and the Linear detail.
- **No speech**: text-to-speech is dropped entirely: no voices, spoken names or alert queue.
- **Q3 refresh**: a summary on every stop, and every 3 minutes while the Dashboard is watched for a running session whose transcript grew.
- **Q4 placement**: the Dashboard is a tab; a rail button or shortcut opens or focuses it.
- **Q5 Needs You**: keep both. The Dashboard groups needs-you sessions among the rest, and the Needs You rail panel stays as the compact always-visible list ([needs-you.md](./needs-you.md)).
- **Q6 answering**: a waiting row shows the question's options as buttons that answer it, driven by the hook's structured question data, falling back to "Go to pane" when the options can't be mapped to keys safely.
- **Q8 cap**: a daily summarizer cap, editable in Settings. The owner's answer named $0.50 for the earlier DeepSeek alert pipeline; the default is set under "Privacy, cache, cap and fallbacks".
- **Provider measurement (RT-27)**: the summarizer's provider follows the smoke of a local small model against Claude Haiku at minimum effort with no thinking.
- ops-tower's Agents-tab rulings (OPS-63, OPS-66 to OPS-69) are under "What ops-tower settled" below.

Settled (owner ruling, 2026-10-28), from the ops-tower absorption mocks (RT-464, [canvases/ops-tower-absorb/RECIPE.md](./canvases/ops-tower-absorb/RECIPE.md)):

- **Default layout, by repo lane**: the Dashboard's default layout is by repo lane, ops-tower's home, lane B of [canvases/ops-tower-absorb/ot-dashboard-additions/index.html](./canvases/ops-tower-absorb/ot-dashboard-additions/index.html).

  A need-you band on top (the waits with their asks, an accounts card, Linear Questions counted by repo), then one lane per repo: a small chart of thin two-hour bars per session or dispatch with gate ticks and a caption ("busy 1h 48m of 2h · gate held now"), the sessions with subagents on the left, that repo's Questions and Next up (three To-do issues and "+N more") on the right.

  Repos with no live agent sit in a "No live agent" table with "Show all lanes".
- **Second view, by status**: lane A (by status, dense rows, as approved) stays as the header's second view.
- **Two-hour activity track**: in. It needs a per-session status-change log that survives a daemon restart, or shows a hatched "no record before HH:MM · the daemon restarted" stretch; dispatch rows read from the dispatch run folder (RT-31).

Out of this design's scope, each with its own design; the Dashboard consumes what they publish, and each DB step names the dependency:

- **Plan tab** (board and daily briefing): a Plan tab from a new rail icon (owner); RT-56, mock [canvases/ops-tower-absorb/ot-plan-board/index.html](./canvases/ops-tower-absorb/ot-plan-board/index.html). The lanes' Questions and Next up and the "No live agent" table read its board.
- **Gates**: a rail view plus a footer strip (owner); RT-55, [gate-dashboard.md](./gate-dashboard.md). Gate ticks and gate rows read its reader.
- **Accounts**: a footer chip and popover plus Settings → Accounts (owner); RT-39 and RT-66, [accounts.md](./accounts.md). The accounts card reads its fleet verdict.
- **Costs**: ops-tower's subscription-share model plus a Costs tab (owner); no issue yet, mock [canvases/ops-tower-absorb/ot-costs/index.html](./canvases/ops-tower-absorb/ot-costs/index.html). The header's Today figure is the API-equivalent sum below until the Costs tab ships (RT-479); then it shows the subscription share with API prices muted beside it, and a click opens the Costs tab, as the costs mock's lane A draws it.

## Layout

Two views of the same sessions, switched by the header's `By repo | By status` toggle: **By repo** (the default, the additions mock's `laneB()`, `bandB()` and `laneRepo()`) and **By status** (the Dashboard mock's lane A, `laneA()`, with the additions mock's `Last 2 h` column).

The Dashboard mock's sample data (`const S`) is a plausible 14:32 with eight sessions in three repos; the additions mock adds the tracks (`const T`), Questions, Next up and accounts, and its state tabs show the quiet and trouble states.

**Orchestrator decision (not an owner ruling):** the switch is remembered per client, in `native-ui.json` (`dashboard.view`, `"repo"` by default).

### Header strip

One band at the top of both views: the title, count pills (`N need you` in the asking colour, then working, idle, done), the day's figures (`Today $X API-equiv`, tokens, commits, issues Done), one progress bar per Linear release the listed sessions' issues belong to (`vNext 11/18`), the Linear sync age (`Linear · 1m ago`, in the danger colour with its age when Linear is unreachable) and the `By repo | By status` toggle. The toggle is not drawn until By repo exists (DB.25); the figures, release bars and sync age stay hidden while the day's totals are zero and the Linear sync time is unset.

### The needs-you count

The rail button's and the tab's badge, and the `need you` pill, count the waiting sessions.

**Orchestrator decision (not an owner ruling):** Linear Questions never count toward it; the accounts card adds one only while every account is dry (blocked now), not while the accounts merely run dry later at the current pace.

### By repo (default)

**Need-you band** (`.band`), three columns, washed in the asking colour while anything waits:

1. **Need you**, longest wait first: per waiting session its glyph, label, repo, state and age, and its ask block with the answer buttons (see "Inline answers"). `No agent is waiting.` when empty.
2. **Accounts**: the card from the accounts feature's fleet verdict: its title (`Accounts run dry Fri 23:22`, or `All accounts blocked now` in the danger colour), when and for how long, the detail line, and `Open accounts`. `The accounts last the week.` when nothing is due.
3. **Questions**, `N · by repo`: one row per repo with its count of Linear Questions (blocked issues waiting on the owner's answer, from the Plan tab's board), dimmed and stamped with the read's age while Linear is unreachable. `No open questions.` when empty.

When nothing waits, the accounts last and no Question is open, the band is calm (no wash).

**Lanes**: a `Lanes · N with a live agent` heading with the track legend (working, asking, idle, finished, gate hold) and `Show all lanes`, then one lane (`.lane`) per repo or workspace with a listed session, in sidebar order:

- **Lane head**: the repo's swatch and name, `N live · M sessions`; the **activity track** chart: one thin two-hour bar per session (labelled by its issue key, else its label) and per dispatch run (labelled by its title, in lilac), over an axis from two hours ago to `now`; the caption; and `Open plan` (the Plan tab on this repo).
- **Caption**: `busy <span> of 2h` (the union of the lane's working stretches), then `gate held now` or `last gate <age> ago`; `since HH:MM` first when the record starts inside the window; `idle for 2h` when nothing ran.
- **Left, sessions**: one row per session (`.brow`): glyph; label and branch; the Linear issue line and title; the state word, its age and stuck flags over the now line (a waiting session's reads `Answer in Need you above`); context; run and last activity; cost. A working session's agent tree nests under it (subagents, dispatch runs tagged `dispatch`, background shells, gates), as in By status.
- **Right, Questions and Next up**: the repo's Questions (issue key, the waiting note, age) on the asking wash, then `Next up · N in To do`: the first three To-do issues in board order (key, priority, title) and `+N more · Open plan`, from the Plan tab's board.

**No live agent**: a table of the repos with open To-do issues and no listed session: repo, its next issue (key and title), `N to do`, `N landed`, its Question count, `Open plan`. `Show all lanes` adds a faded lane for each of them above the table (ops-tower's OPS-179).

### By status

The approved lane A, below.

**Orchestrator decision (not an owner ruling):** in By status the two-hour track sits beside the 10-minute bars as a `Last 2 h` column (a session's own bar and a short caption, `busy 46m · in gate`); the queue strip also carries the accounts chip and a `N questions on Linear · M repos` chip, as the additions mock's lane A draws them.

#### Queue strip

Under the header, while any session waits: one chip per waiting session, longest wait first, each with the asking glyph, the issue key (else the session label), the prompt on one line (a permission reads `<tool>: <command>`) and its age, then the accounts and questions chips. A click on a session chip scrolls its row into view.

**Orchestrator decision (not an owner ruling):** Ctrl+J (unbound in `apps/native/src` today) focuses the pane of the next waiting session, longest wait first, in either view, as a Needs You row click does. The waits come from `needs_you::rows` (`apps/native/src/needs_you.rs:41`), so the band, the queue, the Needs You panel and the Needs you group agree on who waits and in what order.

#### Rows

A sticky column header, then one group heading per group (`Needs you 2`), then one row per session, two lines high (`.ta .row`, `min-height: 46px`):

| Column | Line 1 | Line 2 |
|---|---|---|
| (glyph) | the status glyph (`status_glyph.rs`) | |
| Session | the session's display label | the container's name in its accent, then the branch in mono |
| Linear | the issue key, its state (icon and name) and priority bars | the issue title |
| Now | the state word (Question, Permission, Working, In gate, Idle, Finished, Exited 0, Error) and the time in it, then any stuck flags (`⚑ 3 tool calls failed in a row`) | the **now line** (below) |
| Summary | the newest summary line | the one before it, after its window label (`5–15m`) |
| Agents | `1 sub (1 live) · 1 dispatch · 2 bg`, or `—` | |
| Context | a bar and the percentage, drawn hot past 80% | |
| 10 min | ten one-minute bars of tool calls, oldest first; a quiet session draws them faint | |
| Last 2 h (arrives with DB.22, not DB.12) | the session's activity track | its short caption |
| Run · last | the runtime since the session started | time since its last activity |
| Cost | the session's cost in dollars | its input tokens |

A waiting row is washed in the asking colour with an asking stripe on its left edge.

#### Grouping and order

**Needs you** (status `AwaitingInput`, or `Idle` when the Answer rule holds: summary kind `needs_answer` and made at or after `status_since`, the shared function `needs_you::is_answer`; the Needs You panel adopts it in NY.5, see [needs-you.md](./needs-you.md)), **Working** (status `Working` or `Spawning`, or `Idle` with `background`), **Idle** (the rest of `Idle`), **Done** (`Stopped` or `Error`, or `Idle` with an unseen turn). Ended sessions are in Done and are not waits. An `Idle` session takes the first group that fits, in the order Needs you, Working, Done, Idle. Needs you runs longest wait first, as the Needs You panel orders its asking rows; the other groups newest first by last activity, else `status_since`, else `started_at`.

### The now line

The **now line** says what a session is doing this second, highest rule first (OPS-63, and ops-tower's label rules in `docs/agents-transcripts.md`, "The label"):

1. Waiting on the user: the pending prompt on one line, as the Needs You panel words it (`detail_of`, `apps/native/src/needs_you.rs:101`).
2. Working, held by a gate: `In gate · <kind> <index>: <command>`, or `Queued for gate · #n`.
3. Working on a running subagent's or dispatch run's result: `Waiting on subagent "<description>"`.
4. Working with an open tool call: `Running <tool> · <age>: <command>`, the oldest open call, whitespace collapsed and cut at 80 characters.
5. Working with only background work running, or idle with the snapshot's `background` flag: `Waiting on a background task`.
6. Working otherwise: `Thinking`.
7. Idle with an unseen turn: `Turn finished · not looked at yet`.
8. Idle otherwise: the newest summary's headline, else `Idle`.
9. Ended: `Session ended · exit N`, or `Session ended` when there is no exit code.

A `Spawning` session reads `Starting`. A row in the Needs you group by the Answer rule (status `Idle`) reads `Idle` and its summary headline, even when its turn has not been looked at.

The daemon words rules 2 to 6 (ages `<1m`, `5m`, `1h05m`) in `crates/daemon/src/dashboard/now.rs`. Rules 1, 7, 8 and 9 reach the client as kind `unknown` with empty text and `since` = `status_since`, and the client words them. Rule 1 for the agent tree (a session that is not running) forces Running and Queued subagent and shell nodes to Finished, worded `finished`.

### Nested rows

Under a Working row, and in By status under any row whose drawer is open, one row per node of its agent tree below the main agent, indented by depth with a `└` branch: the node's glyph and name (a subagent's description, a dispatch run's title with a `dispatch` tag, `bg shell <id>`, `Monitor <id>`, `In gate · heavy 1`), what it runs now or its task (`Running Bash · 1m: <command>`, `finished · 2 findings, both fixed`, `exited 1 · 12m`), its own stuck flag, and its age.

In By status a dispatch run's row also carries its own activity track. Finished nodes stay listed, dimmed, while their session's turn lasts.

**Orchestrator decision (not an owner ruling):** subagents get no activity tracks; sessions and dispatch runs do.

### Inline answers

Under a waiting row in By status, and in the band's Need you column in By repo (DB.23), an ask block (`.askb`, `.askin`): `Question ·` or `Permission ·` and the prompt's text; for a permission, the command in a mono box (`$ gh pr merge 118 --squash --delete-branch`); then one button per choice with its key hint (`1`, `2`, `3`; a permission's are `1`, `2` and `Esc`), the first one primary, and a ghost `Go to pane` button last.

A choice button sends `AnswerPrompt`. The daemon types the keys only while the session still waits on the same prompt (same `status_since` and the same `pending_input`) and its screen still shows the agent's dialog; otherwise it refuses and the row redraws from the newer snapshot.

Buttons are offered for a single question with single choice and for a permission prompt. A prompt with several questions, a multi-select question, a plan approval or an elicitation shows its text and `Go to pane` alone (Q6's fallback).

**Orchestrator decision (not an owner ruling):** the keys for each choice come from the agent plugin (`Agent::answer_keys`), since they are its TUI's keys: for Claude, an `AskUserQuestion` option n is the digit n, a permission's Allow once is `1`, its "don't ask again" choice is `2` (offered only when the screen engine reads that choice on the dialog), and Deny is `Esc`.

### Drawer

In By status, a click on a row toggles a drawer docked under it (`.drawer`), three columns:

1. **Running summary**, newest first, with its source and age (`model, 3m ago`): the session's **summary ring**, each line after its window label (`now`, `5–15m`, `15–30m`, `30m+`). Under it, **Prompt**: the session's first prompt on one line.
2. **Recent work**, headed by the changed-file count and lines (the client sums the lines of the files it receives, at most 50, with `files_total` the full count; `4 changed +165 −7`, or `7 files · +212 −48 landed` once the worktree is clean): the commits made since the session started (short sha, subject, age), the issues it moved to Done (`→ Done · <title>`), the uncommitted files (`M`/`A`/`D`, path, `+a −d`), and a pull-request line when the branch has one (`PR #118 · checks passed`). `Nothing yet` when empty.
3. **Linear**, with its sync state: the issue line and title; Project, Release (with its done count), Worktree, Tokens (in and out) and Started; then `Go to pane`, `Open diff` (a diff tab of the worktree) and `Open <key>` (the issue's Linear URL in the browser).

The drawer fetches its detail when it opens and again whenever its session's facts or summary change, at most every 5 s; it is never part of the row stream (OPS-67). A drawer whose session ends keeps what it last showed, marked ended.

**Orchestrator decision (not an owner ruling):** in By repo a click on a session row switches to By status with that row's drawer open, since the additions mock gives lane rows no drawer of their own.

### Which sessions are listed

**Orchestrator decision (not an owner ruling):** the Dashboard lists every session that has an agent (`SessionSnapshot.agent`, `crates/protocol/src/lib.rs:634`), headless runs included; plain shells, parked and abandoned sessions are left out. A busy-excluded session is listed but left out of the header's Working count, as the sidebar's busy counts leave it out.

### Placement

**Orchestrator decision (not an owner ruling):** the Dashboard is a daemon-stored tab kind, `TabContent::Dashboard`, at most one per client layout, opened or focused by `OpenDashboardTab` as `OpenViewerTab` does for a viewer (`docs/native-client.md`, "Tab layouts"; `crates/daemon/src/server/tabs.rs`, `open_dashboard_tab` and `open_viewer_tab`), so it survives restarts with the rest of the client's tabs.

A Dashboard button on the activity rail (not an `Activity` panel, `apps/native/src/sidebar.rs:379`) opens or focuses it and carries the needs-you count as a badge, as does the tab itself. The client sends `WatchDashboard { on: true }` while the tab is the active tab of a shown window and `false` when it stops being one.

## Data sources

What each field needs, and whether the tree has it today (paths at `main` b4b55b37).

| Field | Source | Today | Added by |
|---|---|---|---|
| Label, repo, branch | `SessionSnapshot.label` / `user_label` / `terminal_title` (`crates/protocol/src/lib.rs:587-593`, `:644`), `members[].repo_name` and `branch` (`SessionMember`, `:563-567`); the container accent from the sidebar model | have | |
| Glyph, state word, age | `status` (`:596`), `status_since` (`:599`), `pending_input` (`:708`), `background` (`:713`), the client's `Leaf.unseen` (`apps/native/src/sidebar.rs:196`) | have | |
| Linear key, state, priority, title | issue detection plus the daemon's Linear client and cache, designed in [linear-links.md](./linear-links.md) (RT-133, LL.1 to LL.3) | none: no Linear client in the tree (`rg -il linear crates apps` finds only test strings, a `Linear` split layout and the word "linear" in comments) | RT-133, then DB.7 |
| Now line, rule 1 | `pending_input`, worded by `needs_you.rs:101` | have | |
| Now line, rules 2 to 6 | open tool calls, running subagents and background tasks from the transcript; gate claims from the daemon's gate reader, designed in [gate-dashboard.md](./gate-dashboard.md) (RT-55, re-cut by RT-64 around the `gate-state` crate) | transcript side have: the activity reader's `open_calls`, `subagents` and `background` (`ActivitySnapshot`, `crates/agents/api/src/activity.rs`); no gate reader yet, and the `Stop` hook's `background_tasks` is read only as one flag (`crates/agents/api/src/hook_vocabulary.rs`) | DB.3, DB.4, DB.8 |
| Summary lines | the summarizer over each turn's last message: `SessionRecord.last_assistant_message` (`crates/daemon/src/session.rs:228`, set at `crates/daemon/src/hook_status.rs:505`, 8 KiB cap); `SessionSnapshot.summary` (`lib.rs:602`), filled from the summary ring | have for finished turns (SA.6); working refreshes wait on DB.17 | DB.17 |
| Agents count, nested rows | subagent folders and background-task records in the transcript | have: the activity reader's `subagents` tree (`crates/agents/claude/src/subagents.rs`), `background` and `ended_background`; DB.5 maps them to `AgentNode` | DB.3, DB.4, DB.8 |
| Context | the newest assistant record's `message.usage` over the model's context window | have: the activity reader's `context` and `model` | DB.3 |
| 10 min | `tool_use` blocks' timestamps in the transcript | have: the activity reader's `minute_counts` | DB.3 |
| Run | `started_at` (`lib.rs:604`) | have | |
| Last activity | the newest transcript record's time | have: the activity reader's `last_record_at` (`SessionMetrics.last_activity_at` is still set only by `push_recent_action`) | DB.3 |
| Cost, tokens | `message.usage` of every assistant record (subagents' included) priced at list rates | have: the activity reader's `tokens_in`, `tokens_out` and `cost_usd`, the listed subagents included (`SessionMetrics` is still filled only for headless runs) | DB.3, DB.4 |
| Stuck: waiting on permission | `pending_input` is `Permission` | have | |
| Stuck: tool failures, context high | failed `tool_result` runs and the context figure from the transcript | have: the activity reader's `failure_run` and `context` | DB.3 |
| Inline answer | `PendingInput::Question` with its options and `multi_select` (`lib.rs:723-755`), `PendingInput::Permission { tool_name, summary }` (`:725`); `ClientMessage::SendInput` (`:2309`) types raw bytes only | prompt data have; no checked answer | DB.13 |
| Summary ring | the summarizer's lines, kept per session | have: the ring in `sessions/<id>/summary.json` | |
| Prompt | the transcript's first real user record | have: the activity reader's `first_prompt` | DB.3 |
| Recent work | `git_inspect::commits_since`, `worktree_status`, numstat and shortstat reads, run through `run_git_read` (`crates/daemon/src/git_inspect.rs`), assembled per worktree in `crates/daemon/src/dashboard/work.rs` | have | DB.6 |
| PR line | `gh pr list --head <branch> --state all --limit 10 --json number,state,statusCheckRollup,isCrossRepository` in the worktree (`crates/daemon/src/dashboard/pr.rs`), the first PR that is not from a fork | have | DB.18 |
| Linear detail, releases, sync age | the daemon's Linear cache (RT-133), plus a release-progress query | none | RT-133, then DB.7 |
| Header figures | a day ledger of cost, tokens, commits and issues Done, kept past each session's end | none | DB.9 |
| Two-hour activity track | a status-change log per session and per dispatch run that survives a daemon restart; `status_since` is stamped in `SessionRegistry::update_from` (`crates/daemon/src/session.rs:744`, `:781`) and kept only as the latest change | the latest change only | DB.19, DB.20, DB.22 |
| Gate ticks | gate holds from the gate's journal, matched to a session by working folder (ops-tower's activity route) | none | RT-55 (as RT-64 re-cuts it), then DB.20 |
| Dispatch rows | the dispatch run folder's live-run files and `stream.jsonl`, designed in [dispatch-follow.md](./dispatch-follow.md) (RT-31) | none | RT-31 (DF.1 to DF.5), then DB.21 |
| Accounts card | the accounts feature's fleet verdict (when every account runs dry at the current pace, or that all are blocked now) | none | RT-39 and RT-66 ([accounts.md](./accounts.md)), then DB.23 |
| Questions, Next up, No live agent | the Plan tab's board: blocked issues waiting on the owner, To-do issues in board order, per repo | none | RT-56, then DB.23, DB.24 |

### The transcript activity reader

**Orchestrator decision (not an owner ruling):** one incremental reader of the agent's transcripts, in the Claude plugin (`crates/agents/claude/src/activity.rs`, behind an `Agent::activity_reader` trait method in `crates/agents/api/src/lib.rs`), supplies every transcript-derived field above. The daemon never parses a Claude transcript itself, so the seam rule holds.

It follows ops-tower's reader (`docs/agents-transcripts.md`, "The transcript records read" and "The cache"): per transcript it keeps the byte offset past the last whole line and parses only appended lines; a first read takes the last 1 MiB and drops the cut first line; a file that shrank is read again. It yields:

- open tool calls (a `tool_use` block with no `tool_result` yet), ignoring calls stamped before the session's current process started;
- running background tasks (`toolUseResult.backgroundTaskId`, a background `Agent` launch's `agentId`) until a `<task-notification>` names them;
- the subagent tree (DB.4), from `<project>/<conversation id>/subagents/agent-*.jsonl` and their `.meta.json` subagent sidecars, with [subagent-streams.md](./subagent-streams.md)'s running and finished rules 1 to 5; its SS.1 and SS.2 readers are this reader, built once (`crates/agents/claude/src/subagents.rs`; rule 1, that a session which is not running has no running subagents, is applied by the Dashboard's facts service, DB.5);
  - a subagent sidecar joins the main transcript's agent call by `toolUseId` (any `toolUseId` once the main transcript was read from its tail); one with no `toolUseId` joins through its background launch (a forked skill), else it is listed only while rule 5 says it runs and its file is newer than the process start;
  - nested subagents hang under their launcher by `parentAgentId`; killed, stopped and `stoppedByUser` map to `Failed`, completed to `Finished`;
  - only a `queue-operation` with `operation == "enqueue"` counts as a task notification, and `records::task_notifications` ends a task only on a whole `<task-notification>` element;
- the newest assistant record's `usage` (input, cache creation and cache read tokens) and `model`, giving the context percentage over the plugin's context-window table;
- tokens and cost: every assistant record of the session and its subagents, a streamed message counted once at its largest usage, priced by the plugin's list-price table; the Dashboard labels the sum `API-equiv`, since a subscription is not billed per token (OPS-69);
- ten one-minute counts of `tool_use` blocks ending now;
- the run of failed `tool_result`s (`is_error: true`; a denial or an interrupt neither counts nor resets), three giving the `tool_failures` stuck reason, and the context past 80% giving `context_high`;
- the first real user record (slash commands as their command line, `/clear` and local-command output skipped), cut at 2 KB, for the drawer's Prompt;
- the newest record's time, for last activity;
- the last assistant text, for the working refresh (DB.17).

**Orchestrator decision (not an owner ruling):** context and cost come from the transcript's `usage`, not from Claude Code's statusline JSON (`context_window.used_percentage`, `cost.total_cost_usd`, which ops-tower reads): the statusline is the user's own setting, and the hook settings file a spawn passes (`crates/agents/claude/src/hooks.rs:79`) cannot add a statusline command without displacing it.

### The dashboard facts service (daemon)

`crates/daemon/src/dashboard/` keeps a **session facts** record per listed session and sends it only to connections that watch a Dashboard:

- While at least one connection has sent `WatchDashboard { on: true }`, a task ticks every 2 s: for each listed session it asks the plugin's reader for what changed, builds the now line (rules above), the agent tree, context, tokens, cost, activity counts and stuck reasons, and sends `DashboardFacts` with the changed entries to the watchers. `WatchDashboard { on: true }` is answered at once with every entry (an empty list when no session is listed), and afterwards a watcher gets only the changed entries, at most every 2 s. `on: false` or closing the connection ends the watch (`server/connection.rs`).
- With no watcher the task parks and reads nothing, as the git watcher parks at `Hub.client_count` 0 (`crates/daemon/src/git_watch.rs`). The facts cache is emptied when the last watcher leaves, so the next first watch reads every transcript afresh.
- `GetSessionDetail` answers one session's drawer: its summary ring, prompt, recent work (DB.6), Linear detail (DB.7) and token figures. It answers from the facts cache with `SessionDetail { lines, prompt, work, request_id }`, or `Error "unknown session: <id>"` echoing the `request_id`; `lines` is the session's whole summary ring (SA.6), `work` is read from git (DB.6, "Recent work" below), and the fields DB.7 to DB.9 and DB.21 fill stay empty until those steps land.
- Facts are runtime-only; a restarted daemon rebuilds them from the transcripts on its first tick.

### Linear context

The Dashboard reads Linear through the daemon's one Linear client and issue cache, which [linear-links.md](./linear-links.md) designs for RT-133 (LL.1 to LL.3) and whose owner rulings stand here: the key is `LINEAR_API_KEY` through `user_env::resolve` and the env-secrets store, an identifier counts only when its team key is one of the workspace's (fetched daily), and live identifiers are refreshed every 5 minutes in one batched query, a copy older than 1 minute refetched on demand.

**Orchestrator decision (not an owner ruling):** the Dashboard adds to that client only what its rows need:

- **Detection**: besides the titles and labels LL.3 scans, the session's branch (`feat/rt-443-settings-height` gives RT-443, case-insensitive) and then its first prompt (`/nextup RT-443`). The first match is the row's issue; one issue per session, no manual override. Its identifier joins LL.3's refresh set, and a drawer opening refetches a copy older than 1 minute, as a hover does.
- **Issue fields**: the card's fields (identifier, title, state and its colour, priority, project, release) and the URL; plus each release's done and total counts for the header's bars, one more query batched with LL.3's refresh. Field names are checked against Linear's schema in DB.7's brief; none is measured here.
- **Issues Done**: keys found in the session's branch, first prompt and commit messages since it started that Linear reports completed after `started_at`.
- **Sync age**: the oldest `fetched_at` among the listed sessions' issues.
- **Without a key**: the Linear column shows the detected key alone, and the drawer's Linear column carries linear-links.md's no-key notice (set `LINEAR_API_KEY`).

### Recent work

Read per worktree of the session (`members[].worktree_path`) when its drawer asks:

- commits on `HEAD` since `started_at` (at most 20), with sha, subject and commit time;
- the uncommitted files from `git_inspect::worktree_status` with `git diff --numstat -z --no-renames HEAD` for their lines (an untracked file's lines are counted from disk);
- once every member worktree is clean, at least one has commits and no read failed, the landed totals from `git diff --shortstat <oldest commit since the start>^ HEAD`;
- the issues Done (above) and the PR line (DB.18): the first non-cross-repository PR `gh` lists for the member's branch, open or landed, with its checks read as failed, then pending, then passed (`NEUTRAL` and `SKIPPED` pass, no checks is none); a workspace takes the first member with one; cached 60 s per worktree and branch; no `gh`, a failed or timed-out call, or unparsable output gives no line.

Each read runs with `--no-optional-locks` and `-c core.quotepath=false`, with `--no-ext-diff --no-textconv` on its diffs, and is killed after 10 s; it is cut at 50 files and cached 5 s per worktree. The landed totals start at the oldest commit since the start, diffing from the empty tree when that commit is a root.

### Gates

**Orchestrator decision (not an owner ruling):** gate rows come from the daemon's one gate reader, the one [gate-dashboard.md](./gate-dashboard.md) designs for RT-55 and RT-64 re-cuts around the `gate-state` crate (`github.com/leftos/gate`, which ops-tower pins by rev in its `Cargo.toml:28`). The Dashboard adds only the placement.

A claim or queue ticket is placed under the nearest listed session whose process is its ancestor (`sysinfo`, already a daemon dependency, `crates/daemon/Cargo.toml:67`), and under the subagent whose open `Bash` call's command matches the claim's (ops-tower's `command_matches`).

A machine without the gate shows no gate rows, no gate ticks and no gate rule in the now line.

### Dispatch runs

A **dispatch run** (a `claude -p` implementer run started by the user's `dispatch` skill) is followed by the daemon's dispatch watcher, designed in [dispatch-follow.md](./dispatch-follow.md) for RT-31: its live-run file names its `worktree`, `run_dir`, `pid` and `tool_pid`, and `<run_dir>/stream.jsonl` its progress.

**Orchestrator decision (not an owner ruling):** the Dashboard nests a run under the listed session whose process is an ancestor of its `tool_pid` (the same ancestry rule as gates), else under the session whose worktree holds its `worktree`; its row shows its title, a `dispatch` tag, what it runs now from its stream, and its own activity track. A run no session claims is left out.

### Day ledger

`day-ledger.json` in the config dir: per local date, each session's latest cost and token figures (a figure only rises), the commit shas and the issue keys moved to Done. The header's Today figures sum the current date. Written at most every 30 s while it changed; dates older than 30 days are dropped. It outlives the sessions, so an ended session's cost still counts.

### The status-change log and the activity track

The **status-change log** is the daemon's record of each session's and dispatch run's status stretches, kept whether or not a Dashboard is open, so the two-hour **activity track** is whole when one opens.

**Orchestrator decision (not an owner ruling):**

- **Where**: `activity/<key>.jsonl` in the config dir, one file per session (`session-<id>`) and per dispatch run (`dispatch-<run id>`), one line `{at, status}` appended on every status change. `SessionRegistry::update_from` already sees every change (it stamps `status_since`, `session.rs:781`), so the append sits beside that stamp; a dispatch run's lines come from RT-31's watcher as its run goes from running to ended.
- **Pruning**: lines older than 2 hours are dropped when the daemon starts and whenever a file is rewritten; a file whose session or run has been gone for 2 hours is deleted.
- **Restarts**: `activity/daemon.json` holds this daemon's start time and an alive stamp refreshed every 60 s. On start, the stretch from the previous daemon's last alive stamp to this start is a gap for every key, and a key with no line before this start (a session first seen now) has a gap from the window's start. A gap draws hatched with `no record before HH:MM · the daemon restarted`, and the caption starts `since HH:MM`.
- **Stretches**: `Working` draws working, `AwaitingInput` asking, `Idle` idle, an end (`Stopped`, `Error`, a run ended) closes the track. The client draws the `Idle` stretch after a turn as finished while that turn is unseen on this client (`Leaf.unseen`), since only the client knows what it has shown.
- **Gate ticks**: the gate journal's holds (through the daemon's gate reader, RT-55) whose working folder is inside one of the session's worktrees, as ops-tower's activity route joins them by `cwd`; a hold still running at `now` is drawn brighter.
- **Wire**: ops-tower's segment shape (RT-28's comment carries it): `ActivityTracks { now, hours: 2, segments: [{repo, key, kind: session|dispatch|hold|gap, label, activity, from, to}] }`, sent to a watcher on `WatchDashboard { on: true }` and then only when a stretch opens or closes. An open stretch has no `to`; the client extends it to now and redraws once a minute.

## The summarizer service

`crates/daemon/src/summarizer/` owns every model call: one queue, one cache and one spend ledger. Its consumers are the summary ring, the snapshot's `summary`, the Finished and Needs you toasts, and the Needs You panel's Answer rows and line 2 (RT-423).

### Triggers

- **Stop**: a main-agent `Stop` hook with a non-empty `last_assistant_message`, in an interactive session. A subagent's `Stop` neither reaches the summarizer nor overwrites the session's last message. The message, capped at 8 KiB, joins the session's unsummarized reports; a call starts at once unless the session's previous call finished under 60 s ago, in which case it starts when the 60 s are up and covers every report since (ops-tower's coalescing). Only a billed call opens the window: a template line or a cache hit does not. A session holds at most 12 unsummarized reports, the oldest dropped. With no client connected, or when the request is replaced, the reports stay pending for the next `Stop`; a session gone before its call gets none, and its pipeline entries are dropped on the next report.
- **Working refresh** (DB.17): every 3 minutes while a Dashboard is watched, for a `Working` session whose transcript grew since its last summary, over the transcript's last assistant text. Its line is provisional: the next refresh or stop replaces it.

**Orchestrator decision (not an owner ruling):** questions and permission prompts make no model call. Their text is shown verbatim from `pending_input`, in the ask block and the toast, so a summary would only add latency and cost.

Only interactive agent sessions with hook events are summarized; headless runs and plain shells are not.

### Providers

**Orchestrator decision (not an owner ruling): the default is Claude Haiku 4.5 run through the Claude CLI headless.** It needs no key: it runs on the user's own Claude login. No code calls the Anthropic API directly, and the call lives in the Claude plugin (`crates/agents/claude/src/summarize.rs`, behind an `Agent::summarizer` trait method); the daemon only runs the command the plugin builds.

- **Command**: the plugin's program (so `RUSTLING_TULIP_CLAUDE` applies) with `-p --model claude-haiku-4-5 --effort low --output-format json --no-session-persistence --setting-sources "" --strict-mcp-config --tools "" --disable-slash-commands --json-schema <schema>`, the prompt on stdin, in an empty `<data_local>/summarizer` folder.
- **Environment**: an allow-list (`SystemRoot`, `TEMP`, `TMP`, `USERPROFILE`, `PATH`, `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, each only when set, as ops-tower's `child_environment` plus `PATH`), then `MAX_THINKING_TOKENS=0` and `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`. `PATH` is not a routing variable; an npm install of the CLI needs it to find `node`. No routing variable reaches it, so it always uses the Anthropic login. The four `CLAUDE_*` names on the list carry login and config, not routing, although the provider table names them.

  `--disable-slash-commands`, `--json-schema` with `structured_output`, and the allow-list environment are not yet measured against the real CLI; an ignored live test exists for that.
- **Why `MAX_THINKING_TOKENS=0`**: RT-27 measured that `--effort low` alone does not stop Haiku thinking (one call wrote 1,650 output tokens in 13.1 s of API time); with it a call cost a median $0.0029 at an API p95 of 2.38 s, but the CLI's wall time was p50 4.4 to 11 s, with a p95 of 20.6 to 51.6 s.
- **Reply**: `structured_output`, else the fence-tolerant parse of `result` (the parser is `crates/daemon/src/summarizer/reply.rs`, shared with the DeepSeek path); `total_cost_usd` for the ledger. `is_error`, a login error, a non-zero exit or the timeout is a failure.
- **Timeout and concurrency**: the process tree is killed after 60 s (ops-tower's bound), and one CLI call runs at a time.

**Orchestrator decision (not an owner ruling): DeepSeek Flash is the alternative provider, behind a saved key.** `deepseek-flash` at `https://api.deepseek.com/anthropic/v1/messages` over HTTP (`crates/daemon/src/summarizer/http.rs`), its row (base URL, model, $0.30 / $1.20 per million tokens, default variable `DEEPSEEK_API_KEY`) in a provider table, `crates/daemon/src/summarizer/providers.rs`, listed in `tools/agent-names-allow.txt`.

- **Request**: `x-api-key`, `anthropic-version: 2023-06-01`, one user message, `max_tokens: 4000` and `thinking: {"type": "disabled"}`, since RT-27 saw `max_tokens` 1500 cut 7.1% of replies inside the thinking block. A reply with `stop_reason: "max_tokens"` is a failure. Whether the endpoint honours `disabled` is not measured; SA.3b's ignored live test measures it.
- **HTTP client**: `reqwest` 0.12 with default features off and rustls, in the tree's `Cargo.lock` beside the native app's `zed-reqwest`, checked to pull no `aws-lc-sys` (`crates/daemon/Cargo.toml:84`). RT-133's LL.2 adds the same crate for Linear; whichever lands first adds it.
- **Timeouts**: 3 s connect, 15 s total; one retry after 1 s on a timeout, a connection error, 429 or 5xx, none on another 4xx. A 401 or 403 marks the key `rejected` and stops calls until the key changes. At most four calls in flight.

**Orchestrator decision (not an owner ruling):** a local model is not adopted. RT-27's Granite 4.2 at 3B and 8B agreed with Haiku's kind on 72.2% and 75.0% of samples against a 95% bar ([spikes/local-summarizer.md](../spikes/local-summarizer.md), "Verdict"). A later model that passes is one more provider row (that doc's "What the daemon would need").

### Output and prompt

- **Output**: JSON `{"kind": "needs_answer|working_update|done_waiting", "headline": "...", "notification": "..."}`. `headline` is at most 8 words (the Idle now line, the Needs You panel's line 2); `notification` is at most 100 characters, the summary line and the toast body. A field over its cap is cut at a word boundary; an unknown `kind` makes the reply a failure.
- **Prompt**: v3 of the spike's prompt (`docs/spikes/local-summarizer/prompt.md`), moved to `crates/daemon/src/summarizer/prompt.md` (the template, embedded with `include_str!`; `prompt.rs` shapes the input and fills it). Against that text: no `speech` field, no `Session:` line (no container or session name leaves the machine), a rule to spell out or drop plan and issue numbers, function names and paths (the leaks RT-27 counted), and a `Trigger:` line (`stop` or `working`).
- **Input**: for a stop, the reports since the last call, joined oldest first with a blank line; for a working refresh, the transcript's last assistant text. Strings shaped like secrets (`sk-…`, `ghp_…`, `github_pat_…`, `xox[bp]-…`, `AKIA…`, PEM blocks, an unterminated one redacted to the end) become `[redacted]` first, then the joined text is capped at 6,000 characters: the first 1,000 and the last 5,000 kept with `\n[…]\n` between. The trigger line comes first and the message text is never substituted into the template.

### The summary ring

Each call's `notification` becomes a summary line stamped with the newest report time it covers. A session keeps at most 12 lines or 2 hours, newest first, a provisional line replaced by the next one. The ring and the latest summary are written to `sessions/<id>/summary.json` beside the sidecar, restored on reattach and deleted with the sidecar on a graceful stop, so a daemon restart neither blanks the Dashboard nor pays again.

The ring is pruned (12 lines, 2 hours) on push and on restore only, so a quiet session keeps its lines. `summary.json` is a `SessionSummary` whose `lines` is the whole ring; it is written atomically right after the registry update that pushes the line, only while `meta.json` exists, and never creates the folder. The orphan, reattached and abandoned inserts restore it; a ring the prune empties, or a file that is missing or corrupt, restores no summary. A template line has no kind, the headline `Turn finished` and source template.

One registry update pushes the line, reads the status and decides the alert. `SessionUpdated` follows, then `WaitingAlert`, but only while the session is `Idle` or `AwaitingInput`.

The snapshot's `summary` carries the newest two lines (the row's Summary column); `GetSessionDetail` carries the whole ring.

### Privacy, cache, cap and fallbacks

- **Privacy**: the text goes to the chosen provider under its terms: for Haiku, Anthropic under the user's own Claude account; for DeepSeek, a provider in China. Settings shows one line under the provider choice.

  **Orchestrator decision (not an owner ruling):** summaries start on when the provider is Haiku through the CLI, since it needs no key, and stay off for DeepSeek until a key is saved.
- **Cache**: results cached by (provider, trigger, SHA-256 of the shaped input), 64 entries, so an unchanged working refresh costs nothing. The queue holds one request per session, and a new trigger replaces the queued (not in-flight) one.
- **Spend cap**: a daily cap in dollars (Q8, editable in Settings) over the providers' reported cost, counted per local day in `summarizer-spend.json` in the config dir, `{"date": "YYYY-MM-DD", "spent_usd": n}` written atomically. Working refreshes stop at half the cap; stop summaries become templates at the cap, and one notice tells the connected clients. The cap is soft: a call already started finishes and its cost is counted.

  **Orchestrator decision (not an owner ruling):** the default cap is $1.00, about 340 calls a day at Haiku's measured median of $0.0029 a call.
- **Fallback** (no provider set up, a failure, or the cap): a template, with no model call: kind none, headline `Turn finished`, and the report's first non-blank line cut at 100 characters as the line; `source` says `template`.
- **Startup failure**: if the summarizer fails to start, the daemon keeps running and every summary is a template. A Rejected key returns the template with no call.
- **Idle**: no call is made while no client is connected (`Hub.client_count`).

### Keys

Only the DeepSeek provider takes a key; the Haiku provider uses the Claude login, and Linear's key follows linear-links.md's ruling.

A typed DeepSeek key is stored in **Windows Credential Manager** through the store the env-secrets code already sets up (`keyring-core` and `windows-native-keyring-store`, `crates/daemon/Cargo.toml:81` and `:121`; `env_secrets::init`, `crates/daemon/src/env_secrets.rs:86`), under the same service name rules as env secrets ([env-secrets.md](../env-secrets.md)), user `summarizer/<provider id>`.

- A key is set with a write-only `SetSummarizerKey` and never leaves the daemon again: status carries only `missing`, `stored`, `env_found`, `env_missing` or `rejected`. Its type redacts itself in `Debug` and is never logged.
- A key may instead name an environment variable, read at call time through `user_env::resolve` (`crates/daemon/src/user_env.rs:25`: the process, then the user, then the system scope).
- An approved tailnet client may set a key: a remote connection already grants spawning a shell, so this adds no power.
- Never in `state.json` or `native-ui.json`. `state.json` gains a secret-free `summarizer` block: `enabled`, `provider`, the DeepSeek key source (`stored`, or `env` with a variable name), `daily_cap_usd`.

## Protocol

`crates/protocol/src/lib.rs`. The summary types (`AlertKind`, `AlertTrigger`, `SummarySource`, `PendingAsk`, `SessionSummary`, `SessionSnapshot.summary`, `WaitingAlert`, `Attention.alert_id`, `RepoEntry.spoken_name`, `WorkspaceEntry.spoken_name`) landed with SA.1 (RT-444).

The app is unreleased, so the changes below are made outright. The need-you band's accounts card and the lanes' Questions and Next up add no Dashboard message: they read what the accounts feature (RT-39) and the Plan tab (RT-56) already send every client.

**Orchestrator decision (not an owner ruling):** removed (DB.1), as nothing without speech reads them: both `spoken_name` fields; `WaitingAlert`'s `prefix`, `speech`, `alert_id` and `trigger`; `Attention.alert_id`; `AlertTrigger`; `SessionSummary.pending` and `PendingAsk`, since the Dashboard reads `pending_input`, which carries the structured question.

The shapes DB.2 settled where this section leaves them open (`NowLine`, `StuckReason`, `PromptChoice`, `SummaryTestResult`, `RecentWork`, the summarizer settings and key states, ten one-minute counts for `activity`) are the definitions in `crates/protocol/src/lib.rs`, recorded on RT-482. Until each handler lands, the daemon answers a new request carrying an id with `Error { message: "not supported yet" }`; `WatchDashboard` and `GetSessionDetail` have handlers (`crates/daemon/src/server/dashboard.rs`).

Added and reshaped (DB.2), each new nested enum with `#[serde(other)] Unknown`:

- `SessionSummary { kind, headline, lines: Vec<SummaryLine>, updated_at, source }`, `lines` (newest first, at most two on the snapshot) replacing `detail`; `SummaryLine { text, at, source, provisional }`.
- `DaemonMessage::WaitingAlert { session_id, kind, notification, source }`: sent once per stop summary while the session is `Idle` or `AwaitingInput`, for the toast.
- `ClientMessage::SetSummarizerSettings { settings, request_id }`, `SetSummarizerKey { provider, key: Option<Secret>, request_id }` (`None` deletes), `TestSummary { request_id }` (the daemon summarizes a fixed sample, skipping the cache and counting its spend, and answers the requester alone with `SummaryTest { result, latency_ms, cost_usd, request_id }`, or an `Error` at the cap, with no provider, or when the summarizer failed to start); `DaemonMessage::SummarizerStatus { settings, providers, keys, spend_today_usd, cap_reached }`, sent on connect, on every settings change, key set, delete or rejection, on every billed call and after the startup key check when it changes a key state, and resent to a lagged connection.
- `ClientMessage::WatchDashboard { on }`; `DaemonMessage::DashboardFacts { facts: Vec<SessionFacts>, totals: DayTotals, linear_synced_at }`, to watchers only.
- `SessionFacts { session_id, now: NowLine, agents: Vec<AgentNode>, context_percent, tokens_in, tokens_out, cost_usd, activity: Vec<u16>, stuck: Vec<StuckReason>, last_activity_at, issue: Option<String>, model }`. Its `issue` is the detected identifier; the fields come from RT-133's `LinearIssues` cache.

  Then `AgentNode { key, kind, name, task, activity, state, started_at, stuck, children }`, `kind` one of subagent, dispatch, shell, monitor, gate; `DayTotals { cost_usd, tokens, commits, issues_done, releases }`.
- `ClientMessage::GetSessionDetail { session_id, request_id }`; `DaemonMessage::SessionDetail { session_id, lines, prompt, work: RecentWork, request_id }`; the drawer's Linear column reads the client's copy of RT-133's cache, and the header's release counts ride `DayTotals.releases`.
- `DaemonMessage::ActivityTracks { now, hours, segments: Vec<ActivitySegment> }`, `ActivitySegment { repo, key, kind, label, activity, from, to: Option<DateTime<Utc>> }`, `kind` one of session, dispatch, hold, gap: to watchers only (see "The status-change log and the activity track").
- `ClientMessage::AnswerPrompt { session_id, since, choice, request_id }`, refused with an `Error` carrying the `request_id` when the prompt changed.
- `TabContent::Dashboard`, with a new `#[serde(other)] Unknown` on `TabContent`; `ClientMessage::OpenDashboardTab { id }` answered with `DashboardTabOpened { id, tab_id }`.

## Toasts

The Notifications tab keeps its `Needs you` and `Finished` toggles and its `Skip notifications when` row (`docs/native-client.md`, "Notifications"); the summarizer only improves what the two toasts say:

- **Finished**: while summaries are on and under the cap (`SummarizerStatus`), the client holds a Finished toast up to 12 s for the session's `WaitingAlert`, then titles it by the kind (`<label> needs your answer`, `<label> finished`, `<label> posted an update`) with the alert's `notification` as the body. Only an interactive session that names an agent is held; an alert that arrived before the turn counted as finished words the toast at once; a template alert, or none in time, leaves the toast as today (`docs/native-client.md`, "Notifications").
- **Needs you**: the body is `<label>: <prompt line>` (`detail_of`), with no model call.

## Settings

A **Dashboard** tab after Notifications in `settings_view.rs`:

1. Summaries on / off; the provider (Claude Haiku through the CLI, DeepSeek) with its privacy line; for DeepSeek, the key source (Stored key, Environment variable), a masked field with Save and Clear or a variable-name field, and the key state.
2. The daily cap and today's spend.
3. Test: sends `TestSummary` and shows the reply, its source, latency and cost.
4. Linear: whether `LINEAR_API_KEY` is found (RT-133's `LinearContext`), read-only.

## Mobile

The mobile app's session list (MA6, [mobile-app.md](./mobile-app.md)) can show the same rows compactly from the same snapshot field and facts, with no model call on the phone; its own session screen takes the place of "Go to pane".

## What ops-tower settled

ops-tower's Agents tab is the view this Dashboard absorbs (RT-67, from ops-tower's OPS-70); its designs are quoted in full in the OPS issues named here.

- **Rows and states (OPS-63)**: a deterministic task line per row, and "quiet" replaced by what the row waits on (`Running {tool} · {age}: {command}`, `Waiting on subagent "{name}"`, `Waiting on a background task`, `In gate · {slot}`, `Queued for gate · #n`), with an activity bar of tool calls per minute over 10 minutes. The now line and the 10 min column take these.
- **Stuck flags (OPS-68)**: three consecutive errored `tool_result`s (denials and interrupts neither count nor break the run), context past 80%, and waiting on permission. A flag on the row with its reason.
- **Cost (OPS-69)**: a session's cost includes its subagents, so subagent rows show none; a persisted day total keyed by local date and session id. The header's Today figure and the day ledger use this rule.
- **Recent-action bullets (OPS-66)**: per-window progress lines of at most 120 characters (`last 5m`, `5–15m ago`, `15–30m ago`). ops-tower makes them with Haiku through the CLI (`docs/agents-transcripts.md`, "Turn-end reports"); the summary ring and its window labels follow it.
- **Detail drawer (OPS-67)**: fetched on open, never on the poll; the prompt (2 KB cap), last actions, worktree changes and gate slot; stable row keys (`session:{id}`, `subagent:{agentId}`); an ended row's open drawer stays, marked ended. The drawer follows it.
- **Need-you rail**: a pure reducer (`ui/rail.js` `needYou`) listing agent waits longest first; the need-you band, the queue strip and the Needs you group follow it through `needs_you::rows`. Its account item and Linear questions are the band's accounts card and Questions column, fed by RT-39 and RT-56.
- **Lanes and the two-hour track (OPS-92, OPS-179)**: one lane per repo with a live agent, `Show all lanes` for the rest, and per-session bars from a transition log with gate holds as ticks; the By repo view follows them.
- **No tab bar**: ops-tower's dashboard is its home page. Here the Dashboard is a tab (Q4), whose default view is ops-tower's home.

## Steps

Ids: **SA** for the summarizer service, **DB** for the Dashboard. Each names its files and what it waits on.

- [x] SA.1 Add the summary wire types. Landed as RT-444 (dcf97d78): the types and fields listed under "Protocol".
- [x] SA.2 Store the summarizer's key in Credential Manager. `crates/daemon/src/summarizer/mod.rs` (new module root), `crates/daemon/src/summarizer/keys.rs` (new), `crates/daemon/src/env_secrets.rs` (`service()` and the size constant crate-visible), `crates/daemon/src/main.rs` (its `mod` line): `KeySource` (stored, or a variable through `user_env::resolve`), set / get / delete on the store `env_secrets::init` sets up, the key states, redacting `Debug`.

  Proof: tests on the mock store (set, read back, delete, a missing variable) and that `format!("{:?}")` of a key shows no key text. Waits on nothing.
- [x] SA.3 Run the Claude plugin's summarizer through the CLI. `crates/agents/api/src/lib.rs` (`Agent::summarizer` and its spec type), `crates/agents/api/src/conformance.rs`, `crates/agents/claude/src/summarize.rs` (new: argv, environment allow-list, extracts the reply text from the CLI's envelope), `crates/agents/claude/src/lib.rs`, `crates/daemon/src/summarizer/cli.rs` (runs the spec in the summarizer folder, 60 s kill, one at a time, parses the text with `reply::parse`), `crates/daemon/src/summarizer/mod.rs`, `crates/daemon/src/summarizer/reply.rs` (`SUMMARY_SCHEMA`), `crates/daemon/src/paths.rs` (`resolve_summarizer_dir`), `crates/daemon/src/conversations.rs` (`job` made crate-visible).

  Proof: plugin unit tests on argv, environment (no routing variable passes) and parsing (`structured_output`, a fenced `result`, `is_error`); a daemon test running a fake program. Waits on SA.2 (the module root).
- [x] SA.3b Call DeepSeek's Messages endpoint. `crates/daemon/src/summarizer/http.rs`, `crates/daemon/src/summarizer/providers.rs`, `crates/daemon/Cargo.toml`, `Cargo.lock`, `deny.toml`, `tools/agent-names-allow.txt`: request, fence-tolerant parse, caps, timeouts, the retry rule, `rejected`, cost from `usage`.

  Proof: tests against a local axum mock (200, fenced JSON, 429 then 200, 401, a timeout, an unknown `kind`, `stop_reason: max_tokens`), and an ignored live test that `thinking: disabled` is honoured. Waits on SA.2.
- [x] SA.4 Shape the summarizer's prompt and input. `crates/daemon/src/summarizer/prompt.md`, `crates/daemon/src/summarizer/prompt.rs`, `docs/spikes/local-summarizer.md`: the v3 prompt with no `speech`, redaction, head and tail cap, the trigger line (the schema is `SUMMARY_SCHEMA` in `reply.rs`, from SA.3). Proof: unit tests for each; then rerun the spike's `bench.py` against Haiku with v3 and record the leak count beside v2's. Waits on SA.2 (the module root).
- [x] SA.5 Add the summarizer's settings, spend ledger and cache. `crates/daemon/src/state.rs` (the `summarizer` block), `crates/daemon/src/summarizer/mod.rs` (queue, coalescing, cache, ledger), `crates/daemon/src/paths.rs` (`summarizer-spend.json`), `crates/daemon/src/server/summarizer.rs` (new: `SetSummarizerSettings`, `SetSummarizerKey`, `TestSummary`, `SummarizerStatus`), `crates/daemon/src/server.rs` (`dispatch`), `crates/daemon/src/server/handshake.rs` (status on connect).

  Proof: server tests that a key set is never echoed, the cap switches to templates, half the cap stops refreshes, and `TestSummary` answers only its requester. Waits on SA.2, SA.3, SA.3b, DB.2.
- [x] SA.6 Summarize each finished turn into the session's summary ring. `crates/daemon/src/summarizer/turns.rs` (new: stop trigger, 60 s coalescing, ring, template fallback, `WaitingAlert`), `crates/daemon/src/session.rs` (the ring on the record, the snapshot's two lines), `crates/daemon/src/hook_status.rs` (a `Stop`'s message reaches the pipeline), `crates/daemon/src/orphan.rs` (`summary.json` restored on reattach), `crates/daemon/src/paths.rs`.

  Proof: a daemon test feeding fake `Stop` events against a fake provider, asserting `SessionUpdated` with the line and one `WaitingAlert`, two quick stops making one call, and the ring surviving a reattach. Waits on SA.4, SA.5.
- [x] DB.1 Remove the speech-only wire fields. `crates/protocol/src/lib.rs` and the literal sites the compiler names (`crates/daemon/src/presets.rs`, `registry.rs`, `server/git.rs`, `env_secrets.rs`, `branch_names.rs`, `branch_fate.rs`, `server/spawn.rs`, `server/test_support.rs`, `workspace.rs`, `apps/native/src/launch_last.rs`: one-line deletions): the removals under "Protocol". Proof: the workspace builds and the protocol tests pass with the round-trip tests reshaped. Waits on nothing.
- [x] DB.2 Add the Dashboard's wire types. `crates/protocol/src/lib.rs`, `apps/native/tests/support/mod.rs` (session setters; the facts builder comes with DB.12 or later), `crates/daemon/src/server/test_support.rs`: everything under "Added and reshaped". Proof: a round-trip test per message and type, an `Unknown` test per new nested enum, and a tab list with an unknown tab kind decoding. Waits on DB.1.
- [x] DB.3 Read a session's transcript for the Dashboard. `crates/agents/api/src/lib.rs` (`Agent::activity_reader`), `crates/agents/api/src/activity.rs` (new: the reader's output types), `crates/agents/claude/src/activity.rs` (new: the incremental main-transcript reader), `crates/agents/claude/src/pricing.rs` (new: list prices and context windows), `crates/agents/claude/src/lib.rs`.

  Proof: unit tests over transcript fixtures for open calls, background tasks, usage and cost, minute counts, the failure run (a denial neither counts nor resets), the first prompt, a shrunk file read again. Waits on nothing.
- [x] DB.4 Build each session's subagent tree. `crates/agents/claude/src/subagents.rs` (new: the folder and sidecar reader, `<task-notification>` parsing, subagent-streams' rules 1 to 5), `crates/agents/claude/src/activity.rs` (join into the tree).

  Proof: fixture tests for a running, a finished, a killed and a nested subagent and a background shell ended by its notification. Waits on DB.3. Shared with [subagent-streams.md](./subagent-streams.md)'s SS.1 and SS.2.
- [x] DB.5 Send session facts to watching Dashboards. `crates/daemon/src/dashboard/mod.rs` (new: watcher count, 2 s tick, reader cache, facts), `crates/daemon/src/dashboard/now.rs` (new: the now line's rules), `crates/daemon/src/server/dashboard.rs` (new: `WatchDashboard`, `GetSessionDetail`), `crates/daemon/src/server.rs` (`dispatch`), `crates/daemon/src/main.rs` (`mod` line), `crates/daemon/src/server/connection.rs` (ends a connection's watch), `crates/daemon/src/server/test_support.rs` and `crates/daemon/src/presets.rs` (the `Hub` literal).

  Proof: a server test with a fake reader: facts reach only watchers, only changed entries follow, nothing is read with no watcher, and each now-line rule picks the right text. Waits on DB.2, DB.3, DB.4.
- [x] DB.6 Read each session's recent work from git. `crates/daemon/src/git_inspect.rs` (commits since a time, `--numstat`, `--shortstat`), `crates/daemon/src/dashboard/work.rs` (new: per-worktree read, 50-file cut, 5 s cache), `crates/daemon/src/server/dashboard.rs` (the detail's `work`). Proof: git-fixture tests for commits since start, uncommitted line counts, landed totals once clean, and an in-place session. Waits on DB.5.
- [ ] DB.7 Show each session's Linear issue. `crates/daemon/src/dashboard/linear.rs` (new: detection from the branch and first prompt, the issues Done rule, the sync age), `crates/daemon/src/linear.rs` (RT-133's client: the detected identifiers join its refresh set; release done and total counts in its batched query), `crates/daemon/src/dashboard/mod.rs` (the identifier on facts, `DayTotals.releases`).

  Proof: detection unit tests (a branch, a prompt, a non-team key like `UTF-8` ignored, the branch winning over the prompt); a fake-transport test that a detected identifier is fetched and refreshed and that release counts parse. Waits on RT-133's LL.1 to LL.3, DB.5.
- [ ] DB.8 Show gate holds and queue places under the session that runs them. `crates/daemon/src/dashboard/gates.rs` (new: place each claim and ticket of the daemon's gate state by process ancestry and command match), `crates/daemon/src/dashboard/mod.rs`, `crates/daemon/src/dashboard/now.rs` (the gate rule).

  Proof: placement unit tests over a fake process table and claims (holder under its session, under its subagent by command, a queued ticket's number, no gate state gives no rows). Waits on DB.5 and the daemon's gate reader (RT-55's GD.1 to GD.4, as RT-64 re-cuts them).
- [ ] DB.9 Keep a day ledger for the header's figures. `crates/daemon/src/dashboard/ledger.rs` (new: `day-ledger.json`, rising figures, 30-day prune), `crates/daemon/src/paths.rs`, `crates/daemon/src/dashboard/mod.rs` (`DayTotals`). Proof: unit tests that a figure only rises, an ended session still counts, a new local date starts empty, and a corrupt file starts empty without being overwritten. Waits on DB.5, DB.6, DB.7.
- [x] DB.10 Open the Dashboard as a tab from the rail. `crates/daemon/src/server/tabs.rs` (`OpenDashboardTab`, one per client), `apps/native/src/dashboard_tab.rs` (the tab's rules), `apps/native/src/tab_bar.rs` (the tab's icon and badge), `apps/native/src/activity_bar.rs` (the rail button and badge), `apps/native/src/lib.rs` (routing, `WatchDashboard` on show and hide).

  Proof: a daemon test that a second open focuses the first; a `ui_tabs` spec that the rail button opens and then focuses the tab and sends `WatchDashboard`. Waits on DB.2.
- [x] DB.11 Model the Dashboard's groups and order. `apps/native/src/dashboard.rs` (new, plain Rust: listed sessions, the four groups and their order, header counts, the waits as the Needs you group's rows), `apps/native/src/needs_you.rs` (`is_answer`), `apps/native/tests/support/mod.rs` (session setters), `apps/native/tests/ui/ui_dashboard.rs` (new), `apps/native/tests/ui/main.rs`. Proof: specs over scripted snapshots for group membership and order (the Answer rule included) and busy-excluded sessions left out of the Working count. Waits on DB.2.
- [x] DB.12 Draw the By status view's dense session rows. `apps/native/src/dashboard_view.rs` (new: header strip, queue strip, column header, group headings, two-line rows, context bar, activity bars, nested rows), `apps/native/src/lib.rs` (mounted in the tab body), `apps/native/src/palette.rs` (asking wash, hot context), `apps/native/tests/ui/ui_dashboard.rs`, `apps/native/examples/shot.rs` (a `dashboard` view).

  Proof: specs over scripted snapshots and facts that each column shows its field and nested rows show under working rows; a `native-shot dashboard` before and after. Waits on DB.10, DB.11.
- [ ] DB.13 Answer questions and permissions from the Dashboard. `crates/agents/api/src/lib.rs` (`Agent::answer_keys`), `crates/agents/claude/src/answers.rs` (new: the keys per choice), `crates/daemon/src/server/dashboard.rs` (`AnswerPrompt`: the same-prompt and screen checks, then the keys), `apps/native/src/dashboard_view.rs` (the ask block and buttons), `apps/native/tests/ui/ui_dashboard.rs`.

  Proof: a daemon test that an answer types the keys, and that a changed `status_since`, a changed prompt or a missing dialog refuses it; a spec that a button sends `AnswerPrompt` and a multi-select question shows only `Go to pane`. Waits on DB.2, DB.12.
- [ ] DB.14 Open a drawer under a Dashboard row. `apps/native/src/dashboard.rs` (open drawers, detail requests), `apps/native/src/dashboard_view.rs` (the three columns and their buttons), `apps/native/src/lib.rs` (`SessionDetail`, Open diff, Open issue), `apps/native/tests/ui/ui_dashboard.rs`. Proof: specs that a row click sends `GetSessionDetail` and draws the reply, a second click closes it, an ended session's drawer stays marked ended, and the buttons route. Waits on DB.12, SA.6, DB.6, DB.7.
- [ ] DB.15 Add the Dashboard settings tab. `apps/native/src/settings_view.rs` (the tab), `apps/native/src/text_input.rs` (a masked mode), `apps/native/src/lib.rs` (`SummarizerStatus`, `SummaryTest`), `apps/native/tests/ui/ui_settings.rs`. Proof: specs that Save sends `SetSummarizerKey` and clears the field, the key state follows `SummarizerStatus`, the Linear line follows `LinearContext`, and Test shows the reply. Waits on SA.5, RT-133's LL.1 and LL.3.
- [x] DB.16 Word the toasts from the summaries. `apps/native/src/notify.rs`, `apps/native/src/lib.rs` (the held Finished toast, the Needs you body), `apps/native/tests/ui/ui_notifications.rs`. Proof: specs that a Finished toast waits for its `WaitingAlert` and takes its title and body, falls back after the hold, and a Needs you toast carries the prompt line. Waits on SA.6.
- [ ] DB.17 Refresh working sessions' summaries while the Dashboard is watched. `crates/daemon/src/summarizer/refresh.rs` (new: the 3-minute tick, provisional lines), `crates/daemon/src/dashboard/mod.rs` (the reader's last assistant text). Proof: a paused-clock test that an unchanged transcript makes no call, a grown one makes one, and none runs with no watcher or past half the cap. Waits on SA.6, DB.5.
- [x] DB.18 Show each session's pull request in its drawer. `crates/daemon/src/dashboard/pr.rs` (new: `gh pr list --head <branch> --state all --limit 10 --json number,state,statusCheckRollup,isCrossRepository` in the worktree, 60 s cache, only while watched; no `gh` gives no line), `crates/daemon/src/dashboard/work.rs`. Proof: parse tests over recorded `gh` output and a missing `gh`. Waits on DB.6.
- [x] DB.19 Keep a status-change log per session that survives a daemon restart. `crates/daemon/src/activity_log.rs` (new: the `activity/<key>.jsonl` append, the 2-hour prune, `activity/daemon.json` with its start time and 60 s alive stamp, the restart gap), `crates/daemon/src/session.rs` (an append beside each `status_since` stamp), `crates/daemon/src/paths.rs` (the `activity/` folder), `crates/daemon/src/main.rs` (`mod` line, the alive task, the startup prune).

  Proof: unit tests over a scratch folder and an injected clock: appends follow status changes, old lines prune, a restart after a stale alive stamp yields one gap, a key first seen after a start is gapped from the window's start, a file goes 2 hours after its session. Waits on nothing.
- [ ] DB.20 Send the two-hour tracks to watching Dashboards. `crates/daemon/src/dashboard/tracks.rs` (new: segments and gaps from the log, gate holds from the gate journal by working folder), `crates/daemon/src/dashboard/mod.rs`, `crates/daemon/src/server/dashboard.rs` (`ActivityTracks` on watch and on each stretch change).

  Proof: server tests that a watcher gets every segment on `WatchDashboard`, then one message per stretch change, a gap segment after a restart, and a hold inside a session's worktree joined to it. Waits on DB.2, DB.5, DB.19; gate holds on the daemon's gate reader (RT-55, as RT-64 re-cuts it), with none drawn until it lands.
- [ ] DB.21 Nest dispatch runs under their session and track them. `crates/daemon/src/dispatch_runs.rs` (RT-31's watcher appends each run's status changes to the log), `crates/daemon/src/dashboard/mod.rs` (a run's node under the session that started it, by `tool_pid` ancestry, else by worktree), `crates/daemon/src/dashboard/now.rs` (rule 3 for a run), `crates/daemon/src/dashboard/tracks.rs` (dispatch keys).

  Proof: tests with a fake process table and live-run files: a run nests under its launcher, a run no session claims is left out, its stretches reach `ActivityTracks`. Waits on RT-31 (DF.1 to DF.5), DB.19, DB.20.
- [ ] DB.22 Draw the two-hour activity track. `apps/native/src/activity_track.rs` (new: bars, gate ticks, the hatched gap and its note, the legend, the long and short captions), `apps/native/src/dashboard.rs` (the client's tracks from `ActivityTracks`, an open stretch extended to now, finished while unseen), `apps/native/src/dashboard_view.rs` (By status's `Last 2 h` column, a dispatch row's track), `apps/native/src/palette.rs`, `apps/native/tests/ui/ui_dashboard.rs`.

  Proof: unit tests for the busy union, `gate held now` and `last gate N ago`, `since HH:MM` and `idle for 2h`; a spec that scripted segments draw in the column and an unseen turn's idle stretch draws finished. Waits on DB.12, DB.20.
- [ ] DB.23 Draw the need-you band. `apps/native/src/dashboard_band_view.rs` (new: Need you with ask blocks, the accounts card, Questions by repo, the calm state), `apps/native/src/dashboard.rs` (the band model and the needs-you count rule), `apps/native/src/activity_bar.rs` and `apps/native/src/tab_bar.rs` (the badge follows the rule), `apps/native/src/dashboard_view.rs` (the queue strip's accounts and questions chips), `apps/native/tests/ui/ui_dashboard.rs`.

  Proof: specs that waits run longest first with working answer buttons, Questions never raise the badge, a dry fleet adds one and a later run-dry does not, and the calm state shows with nothing due. Waits on DB.11, DB.13, the accounts fleet verdict (RT-39 and RT-66, [accounts.md](./accounts.md) CA.4 and CA.5) and the board's Questions (RT-56).
- [ ] DB.24 Draw the repo lanes, the default view.

  `apps/native/src/dashboard_lanes.rs` (new, plain Rust: lanes in sidebar order, live and total counts, captions, the No live agent table, Show all lanes), `apps/native/src/dashboard_lanes_view.rs` (new: lane head with chart and caption, session rows with nested rows, Questions and Next up, the No live agent table), `apps/native/src/lib.rs` (mounted; `Open plan` opens the Plan tab), `apps/native/tests/ui/ui_dashboard.rs`, `apps/native/examples/shot.rs` (a `dashboard-repo` view).

  Proof: specs that a repo with a listed session gets a lane, one without lands in the table, Show all lanes adds it faded, and Next up shows three issues and `+N more`; a `native-shot dashboard-repo` before and after. Waits on DB.22, DB.23, the board's Next up and To-do counts and the Plan tab (RT-56).
- [ ] DB.25 Switch between By repo and By status, remembered per client. `apps/native/src/dashboard.rs` (the view), `apps/native/src/dashboard_view.rs` (the header toggle), `apps/native/src/sidebar.rs` (`native-ui.json` `dashboard.view`, `"repo"` when missing or unknown), `apps/native/src/lib.rs` (a By repo row click opens By status with that row's drawer), `apps/native/tests/ui/ui_dashboard.rs`.

  Proof: specs that the Dashboard opens By repo by default, the toggle switches and survives a reload of the UI state, and a lane row click lands on its drawer. Waits on DB.14, DB.24.
- [ ] SA.11 Document the summarizer and the Dashboard.

  `CLAUDE.md` (the on-disk list gains `summarizer-spend.json`, `day-ledger.json`, the `activity/` folder and `sessions/<id>/summary.json`; `native-ui.json` gains `dashboard.view`; `state.json`'s `summarizer` block; the no-direct-API line names the DeepSeek summarizer's HTTP call).

  Also `docs/architecture.md` (task index rows), `docs/native-client.md` (a Dashboard section), `docs/README.md` (glossary), `docs/env-secrets.md` (the summarizer key); the design's lasting decisions promoted and this file deleted. Waits on every step above.
