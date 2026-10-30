# Off Orca: remote clients, attention streams and session views

The owner runs Orca today and will leave it once rustling-tulip covers three things: the native desktop client as the daily driver, a remote desktop client (the same native client connected to the home daemon from another PC), and a mobile app. ops-tower's dashboard is absorbed into the desktop and mobile clients over time. This file is the brainstorm and the roadmap behind the Linear waves; each idea has an id (`PF.n`, `AT.n`, …) that its Linear issue names.

## Rulings

- **The bar for leaving Orca:** phone status and reply, the desktop terminals and worktrees, and remote access from another PC. The chat view is wanted but is not on that bar.
- **Order:** interleaved. Hook status, then the desktop attention feed, then the remote desktop, then the iPhone PWA, with the small desktop parity waves between them; Phase 5 (drag-and-drop, pop-outs) after the iPhone PWA.
- **Chat view:** a mirror over the live terminal session (SV.1), not the stream-peer view switch.
- **Mobile:** a PWA on the owner's iPhone, served by the daemon over Tailscale HTTPS ([mobile-app.md](./mobile-app.md)); no store app and no iOS build pipeline.
- **Reach:** Tailscale only ([remote-access.md](./remote-access.md)); no relay, pinned TLS or mDNS.
- **Progress updates** reach the phone as a silent feed only, never as a push.
- **Clear and continue** runs only on a click, never on its own.

## Roadmap

The waves in Linear team RT, in working order (snapshot: [MAIN.md](./MAIN.md)). Waves 1a to 5 are the path off Orca: Waves 1a–1c finish work already in flight, then the native client becomes a daily driver (Wave 2), with the remote client (Wave 3) and the mobile app (Wave 4).

| Wave | What | Issues from this file |
|---|---|---|
| 1a, 1b, 1c | DeepSeek sessions, retiring the Tauri app, busy tracking | |
| 2 | Daily driver: close the port's gaps (hook status, tabs and panes, forms, repos, containers and presets, Phase 5a drag-and-drop) | RT-84 (AT.1), RT-85 (AT.2) under RT-25 |
| 3 | Cutover: remote mode over Tailscale, file fetch, installer and `v0.1.0-alpha` | RT-96 (MA3) |
| 4 | Mobile app (iPhone PWA over Tailscale) | RT-104 (MA.b), RT-106 (MA.e) and the PW steps in [mobile-app.md](./mobile-app.md) |
| 5 | Attention feed on the desktop (`feat/attention-feed`) | RT-87 (AT.3), RT-88 (AT.4), RT-89 (AT.5), RT-90 (AT.7, AT.8), RT-91 (SV.5), RT-92 (SV.6), RT-93 (fleet) |
| 6 | Remote enhancements | RT-97 (PF.1–PF.3), RT-98 (PF.4), RT-99 (PF.5), RT-100 (PF.8), RT-101 (PF.12), RT-102 (RD.4) |
| 7 | Pop-out windows (Phase 5b) | |
| 8 | Chat mirror and phone session views (`feat/chat-mirror`) | RT-51 (SV.1, SV.2), RT-107 (SV.3), RT-108 (MA8), RT-109 (MA9), RT-110 (SV.7) |
| 9 | Summaries, spoken alerts and the Dashboard | RT-111 (SV.4) |
| 10, 11 | Subagent streams; dispatch runs | |
| 12 | Claude accounts and artifacts by account | RT-112 (MA10), RT-113 (AR.1–AR.3), RT-114 (AR.4, AR.5), RT-115 (PF.14) |
| 13 | ops-tower views: gates, board and dock | (OT) |
| 14, 15 | Dictation; Linear issue links | |
| Ideas, Backlog | | RT-118 (RD.5), RT-119 (RD.6), RT-120 (RD.7), RT-121 (PF.9), RT-122 (PF.10), RT-123 (PF.11), RT-124 (PF.13), RT-125 (MA.f) |

Build tooling's RT-116 (affected-crate tests per item, the full suite at checkpoints and in CI) shortens every wave.

## Where things stand

- **Desktop:** 109 of 150 parity lines done (`native-client-parity.md`). Open: repos/containers/presets and drag-and-drop (Phase 5a), both in Wave 2; remote (Phase 6, Wave 3); pop-outs (Phase 5b, Wave 7); a few keyboard and toast lines.
- **Remote:** TS.1 and TS.2 landed (`crates/tailnet`, `crates/daemon/src/tailnet_listener.rs`; commits 4d18a51, 6e1c847, dffb25d, 08ab90b), with FT.1 `FetchFile` streaming.

  TS.3 and TS.4 start and stop the listener live from `remote.json` and `ConfigureRemote` (off by default; `{enabled, port}`), and TS.5 retires the LAN listener, `/pair` and mDNS. The native client dials only `ws://127.0.0.1` (`apps/native/src/net.rs:285`); no host profiles or remote mode yet (RT-33, RT-34). No upload direction exists at all.
- **Links:** Ctrl+click only; regex over grid rows with soft-wrap and box-border stitching up to 64 rows (`links.rs:105`, `term.rs:931`); resolved client-side against the session cwd, then member worktrees (`open.rs:238-346`), first existing candidate wins.
- **Attention:** one `Attention { reason }` message (`AwaitingInput | Stopped | Error`, heuristic-driven), one Windows toast — `Needs you`, with the session's label as its body, one toggle and the skip-when setting (TN.1/TN.2, `apps/native/src/notify.rs:142`) — and the Needs You rail (NY.1–NY.3). No click action, no sound, no phone; hook status is delivered for Claude Code sessions and opted-in Codex runs (RT-25).
- **Transcript JSONL:** read only for resume candidates (`crates/agents/claude/src/transcripts.rs`); nothing extracts tool calls, cost or tokens for interactive sessions.
- **Orca, for the bar to clear:** phone app (status, scrollback, reply, chat UI, push "agent finished", file tree, source control, account switch, quick commands, attach photo, dictate); desktop Chat UI as a view over the same PTY (`orca/docs/site/content/docs/agents/native-chat.mdx`); a notification bell with unread state.

## 1. Files across machines (`PF`)

Goal: Ctrl+click a path in a remote session and the file opens on the machine you sit at, exactly as it does locally, even when the path on screen is partial.

### Resolving the path

- **PF.1 Resolver on the daemon.** Move candidate resolution from the client (`open.rs`) to the daemon behind one request (`ResolvePath { session_id, token, context }` → ranked candidates). The daemon has the filesystem, the session's cwd history, its worktrees and its transcript, so local, remote desktop and phone all get the same answer from one code path; the client keeps only opening.
- **PF.2 Context the client sends.** The clicked token with its `:line[:col]`, plus the logical lines above it (the stitched window `term.rs` already builds, widened to the last ~200 rows or the current turn), so the daemon can find the anchors below.
- **PF.3 Tiers, in order.** Each tier yields readings; the first tier with exactly one reading on disk wins, a tier with several opens the picker (PF.5), an empty one falls through.
  1. **Normalize:** strip backticks, quotes, a `[..](..)` wrapper (keep the target), trailing `.,;:)`, and `\\?\`; parse `:L`, `:L:C`, `:L-L`, `:L–L`, `(L,C)`, `#L`, `#L-L`, `line L` and the first number of `:L, :L` lists; map `/x/…`, `/d/…`, `/c/…`, `/mnt/x/…`, `~`, `%LOCALAPPDATA%`, `$env:…`, `$HOME`, `file:///` and doubled backslashes; treat `D:\{p}` and `X:\dev\{p}` as one file.

     Placeholders (`{name}` in angle brackets), globs and brace lists are not links; URLs, `crate::path`, host:port, git ranges and version numbers are not paths.
  2. Absolute (UNC under the existing allowlist).
  3. A Markdown link target, relative to the `.md` file whose content is on screen.
  4. The session's current cwd, then the cwd it had when the line was printed (the transcript's per-event `cwd`; a bare `cd` persists between calls).
  5. The nearest preceding directory switch in the same tool block (`cd`, `Set-Location`, `pushd`, `git -C`, `--dir`, `--manifest-path`, `-WorkingDirectory`), with `VAR=…` assignments in the same command expanded; for `git status` and `git diff` output, that directory's repo top.
  6. Siblings and ancestors of earlier full paths in the same message, most recent first; a bare `:L` reuses the most recent file named.
  7. Repo-qualified prose ("yaat-server `DtoConverter.cs:876`"): that repo's root, then its worktrees.
  8. Session roots: member and session worktrees, then each one's main checkout (a worktree removed after landing falls back to the same path in the main checkout), then sibling repos the session touched.
  9. Index lookup: files the session read or edited (transcript, most recent first) by suffix, then a basename or suffix search in the git index of every session root; ties go to touched by the session, then sibling of an earlier mention, then the shallowest path, then the picker.

  A token found nowhere reports "not found in {roots}" rather than guessing: about a fifth of bare names in prose name files outside the session's repo or files that exist only at runtime.
- **PF.4 The transcript as a side channel.** The daemon tails the session's `~/.claude/projects/<cwd>/<id>.jsonl` and keeps a per-session index of every `file_path` a Read/Edit/Write/Grep touched and every Bash `cwd`. A token matching an indexed path's suffix resolves exactly, with no screen scraping; this is what makes "the files Claude just edited" always clickable.
- **PF.5 Ambiguity picker.** Two or more surviving candidates show a small menu under the click (repo-relative paths, the anchor rule that found each), instead of opening the first.
- **PF.6 Plain-click and hover underline** (RT-58) apply to the same resolver.

### Moving the bytes

- **PF.7 Fetch and open** (FT.2/FT.3, RT-35/RT-36): download into a per-host mirror that keeps the repo layout, open with the `:line` rule.
- **PF.8 Cache by hash.** `FileFetchStarted` gains the host file's hash and mtime; a click on an unchanged file opens the cached copy at once.
- **PF.9 Peek.** Hover (desktop) or tap (phone) shows the lines around `:line` in a popover from a ranged read, without downloading; the phone's default action is a syntax-highlighted viewer with Share and Save.
- **PF.10 Edit and send back.** A fetched copy is watched; saving it offers "Send back to host", refused if the host file changed since the fetch (hash check), with a diff.
- **PF.11 Open in VS Code Remote.** When the host is reachable over SSH or a VS Code tunnel, a click can open `code --remote ssh-remote+<host> <path>:<line>` instead of copying; a per-host setting.
- **PF.12 Upload: paste and drop into a remote session.** Pasting an image (Claude Code's own image paste reads the host's clipboard, so it fails remotely) or dropping a file uploads it into `<worktree>/.tmp/rt-uploads/` on the host and types its path into the prompt. The phone's camera and file picker do the same (iOS has no Web Share Target).
- **PF.13 Fetch the files a message names.** A turn-end card ("3 files edited") offers "Fetch all" as one folder.
- **PF.14 Extra roots.** Confinement stays (repo roots and worktrees), plus host-approved extra roots (for example `~/.claude` for skills and plans), set on the host only.

### Evidence from transcripts

Mined from 74 recent Claude Code transcripts across 14 projects (2,404 assistant text blocks, 4,179 shell calls, 9,050 tool results) and two raw scrollbacks of real sessions:

- **Bare names after a full path.** Of 123 bare basenames that follow a full path in the same message, 88 exist in the repo; the sibling rule finds 37 of them (42%), a repo-wide basename index finds 81 uniquely (92%). So tier 9 does most of the work for bare names and tier 6 settles ties. About 10% of found basenames are ambiguous (`CLAUDE.md`, `README.md`, `Cargo.toml`, `SKILL.md`). Agents also write a bare `:500` for a line in the file just named (69 times).
- **Directory switches.** 43% of shell calls switch directory (2,992 switches); only 18% target the session cwd, about 42% a worktree, the rest other repos or `~/.claude`. Targets are spelled MSYS-style or through a variable as often as not. Against the cwd at the time of the call, 81% of `rg path:line` lines and 73% of `git status` lines resolve; the `cd` target adds 3% and 6%, the repo top 7% more for git paths.
- **Worktrees.** 23% of edits land in a worktree while the session cwd is the main checkout; one repo shows up under five spellings in one session.
- **What the screen hides.** Tool headers show cwd-relative backslash paths (`Update(docs\plans\MAIN.md)`) while the transcript holds the absolute path; reads collapse to "Read 1 file" with no path; single-file greps print only line numbers (913 results); long commands are cut with `…`; Claude Code hard-wraps inside paths, once with a diff's line-number gutter between the halves; rows are padded to full width, so the soft-wrap flag cannot be trusted.

  The transcript has every one of these exactly (PF.4), which is why the resolver runs on the daemon with the transcript beside it.
- **Traps.** Placeholders, globs, trailing punctuation, dotted real file names (`CrcClientState.Eram.FlightData.cs`), Rust paths (`git::tests::…`), host:port, git ranges, version numbers, product names with source extensions (`llama.cpp`), runtime-only files (`native-ui.json`), and issue ids beside paths.

## 2. Attention streams (`AT`)

Goal: know when you are needed, on whatever screen you are near, without being pinged for things that are not yours to act on. Orca's "agent finished" fires on every `Stop`, even while subagents or background shells still run; here a `Stop` is classified before anything is sent.

### What a session can be in

- **AT.1 A richer status.** Beside `Working / AwaitingInput / Idle`, the daemon knows a turn ended **with work still running**: `Stop.background_tasks` non-empty, live subagents (`SubagentStart` without `SubagentStop`, or RT-30's subagent tracking), a pending `ScheduleWakeup`/loop, or child processes of the agent still alive.

  That session shows a **background** state (its own glyph: a ring with a spinning arc) and never counts as finished. A `Stop` with `background_tasks` or a one-shot cron is done (RT-84, under RT-25); the other sources listed above are not read yet.
- **AT.2 Hand-off ready.** The orchestrator's checkpoint ends its turn with the exact line `Run /clear, then /nextup to continue.` (nextup `SKILL.md:208`), after writing `~/.claude/handoffs/nextup-<repo>.md` (header `<!-- nextup-handoff written=… expires=… -->`), and the context-budget hook pauses with "…run /clear, then /nextup" (`~/.claude/hooks/context-budget.ps1`).

  The daemon matches the marker in `Stop.last_assistant_message` (or the transcript's turn-end text) on an idle session with no background work and sets a **hand-off ready** flag (RT-85). The marker strings live in one table so `/pickup` and other loops can be added.

### The streams

| Stream | What lands in it | Default delivery |
|---|---|---|
| **Needs you** | permission prompt, AskUserQuestion, plan approval, a prose question at a stop (summarizer `needs_answer`), error or crash | toast with actions, sound, phone push, Needs You badge |
| **Hand-off ready** | AT.2 | toast and push with a "Clear and continue" action |
| **Progress updates** | a stop while background work runs, `working_update` summaries, a subagent or background task finishing | silent feed; batched digest on the phone at most every N minutes; never a toast by default |
| **Done** | a stop with nothing running and nothing asked (`done_waiting`) | badge and feed; optional quiet toast |
| **Fleet** | stuck agent (3 failed tool results, context over 80%), accounts running dry (claude-accounts fleet verdict), gate queue stalled, inbox requests waiting per repo, daemon or tracer trouble | badge and feed; push only for accounts running dry |

- **AT.3 One event feed on the daemon.** Every stream item is an `AttentionEvent { id, session_id, stream, kind, title, detail, actions, created_at, resolved_at }`, kept in a bounded ring (and a file, so a restart keeps unread state), broadcast to every client and to the push sender. A resolution (answered, cleared, dismissed) is broadcast too, so answering on one device clears it everywhere.
- **AT.4 Needs You grows into the inbox.** The rail view lists all streams with tabs or filters, unread state, mark-unread (Orca's bell), and the longest wait first inside Needs you.
- **AT.5 Actionable notifications.** Windows toasts carry buttons: Allow / Deny for a permission, one button per option for a single-choice question (up to 3), inline reply for free text (WinRT toast input), "Clear and continue" for a hand-off, "Open" for everything. Multi-choice and multi-question prompts open the chat card instead. The phone's notifications have no buttons (Safari supports no notification actions), so a tap opens the session's prompt sheet.
- **AT.6 Presence routing.** At the desk (the native client has focus, or input in the last N seconds) → desktop only; desk idle or locked for 3 minutes (Orca's rule) → phone too; a remote desktop client that has focus counts as presence for its machine. Quiet hours per stream.
- **AT.7 Sound and speech.** A sound per stream (Orca lets you pick files); spoken summaries stay RT-26.
- **AT.8 Taskbar.** The taskbar icon gets an overlay badge with the Needs-you count and flashes once on a new item.
- **AT.9 ops-tower's grace rules.** Hold a permission wait 6–15 s before alerting (the user may be answering), drop late tool hooks after an interrupt (Orca's 15 s latch), longest-wait-first by first-seen time.

## 3. Session views (`SV`)

Every Claude session can be shown three ways, on every client: **Terminal**, **Chat**, **Summary**. Each pane and each phone screen picks its own view; the session does not change.

- **SV.1 Chat as a mirror of the terminal session** (ruled; replaces the stream-peer switch in `conversation-view.md`). Orca's Chat UI is "a structured transcript + composer for the same PTY": the terminal stays the source of truth.

  Here: entries come from tailing the session's transcript JSONL (the tailer and parser are `conversation-view.md` CV.2 and CV.3), prompts come from hook `pending_input` (RT-25), messages are typed into the PTY (bracketed paste + Enter), and answers are typed as keys (Orca's rule: the option's number, never its label; the "Type something" row then text; multi-select toggles then Tab to Submit; `orca/src/shared/native-chat-ask.ts:200-280`).

  The desktop can show the terminal while the phone shows the chat of the same session at the same moment, which the stream-peer design (one child at a time, a view switch that restarts the agent) cannot. The stream peer stays a later option for headless-style sessions.
- **SV.2 Interactive cards.** Permission (Allow once / Allow always when offered / Deny with a reason), AskUserQuestion (single choice, multiple choice, free text, several questions with a Submit), plan approval (Approve / Keep planning). An answered card collapses to its answer; a card answered elsewhere (terminal, other device) closes with "answered on {device}".
- **SV.3 Running-work strip.** Over the composer in Chat and Summary: "2 subagents · 1 background shell (`cargo test`, 3m) · wakeup in 4m", each tappable (a subagent opens its stream, RT-30). The strip is the AT.1 state made visible, so a quiet chat is never mistaken for an idle session.
- **SV.4 Summary view.** One line per turn or per window of activity, written by the summarizer (ops-tower's Haiku progress lines: last 5 min / 5–15 / 15–30; the summarizer's `notification`), with the open question pinned at the top and every line expandable into the full chat for that stretch. The phone opens sessions here by default.
- **SV.5 Clear and continue.** A button in the chat header, the summary header, the session menu and on the hand-off notification; it runs only on a click, never on its own.

  It refuses unless the session is hook-confirmed idle with no background work (AT.1), types `/clear`, waits for `SessionStart source=clear` (HS.7), then types `/nextup` (the command comes from the marker table, AT.2). A `/clear` typed by hand during the action does not trigger a second one.
- **SV.6 Session vitals.** Context % with the 40% `/nextup` line marked, cost, and the prompt-cache expiry countdown, on each sidebar leaf, chat header and phone card. Source: the per-session `statusline.json` (only it carries the cache expiry), or the transcript's `usage`. This also fixes `CLAUDE.md:135`'s claim that cost is tailed today.
- **SV.7 Composer niceties** (from Orca): `/` catalog of commands and skills, `@` file mentions, image attach (uses PF.12), quick replies ("continue", "yes"), model / effort pills, dictation (RT-75, MA11).

## 4. Remote desktop client (`RD`)

- **RD.1 Tailscale only.** The daemon's tailnet listener serves its `tailscale cert` TLS over the tailnet (`crates/daemon/src/tailnet_listener.rs`), reachable from anywhere once remote access is on, so the connection picker takes a MagicDNS name. The relay is dropped with the Tailscale-first ruling (`remote-access.md`).
- **RD.2 Phase 6 as planned** (RT-33, RT-34): shared client core, connection picker, pairing, host profiles, remote mode.
- **RD.3 Same notifications remotely.** The remote native client is a full attention client (AT.3–AT.8), with presence per machine.
- **RD.4 Resume without replay.** Reconnect sends each pane's last sequence number; the daemon sends only the missing bytes, so a flaky link does not reload 2 MB of scrollback per pane.
- **RD.5 Several hosts at once.** The sidebar groups by host; one attention feed across hosts. (Orca's phone shows every connected host in one list.)
- **RD.6 Remote directory browser.** Add repo and the folder pickers browse the host's filesystem over the wire, so remote mode does not lose repo management.
- **RD.7 Bandwidth.** `permessage-deflate` on remote connections; coalesce output frames per pane at ~16 ms.

## 5. Mobile app (`MA`, extends `mobile-app.md`)

- **MA.a An iPhone PWA.** The daemon serves the app over its tailnet listener and the owner installs it from Safari to the Home Screen: no store, no Apple Developer account, no macOS CI. Steps PW.1 onward in `mobile-app.md`.
- **MA.b Home is the attention feed** (AT.3), Needs you first, then hand-offs, then sessions by repo with status and the AT.1 background glyph.
- **MA.c Session screen** opens in Summary (SV.4), switches to Chat (SV.1) or Terminal (MA8) per session.
- **MA.d Web Push** straight from the daemon with the owner's VAPID keys, opening the session on a tap; the Home Screen badge counts Needs you (`mobile-app.md`, Push and Badge).
- **MA.e Files:** a path tap opens the peek viewer (PF.9); a file tree per worktree; Share/Save; the page's camera and file picker send images and files into a session (PF.12).
- **MA.f Launch from the phone:** "Launch last again", a preset, or "/nextup in {repo}" in a new session.
- **MA.h Reach:** Tailscale only (RD.1).

## 6. Claude artifacts across accounts (`AR`)

The owner rotates six Claude logins with claude-swap; an artifact a session publishes belongs to the login that session ran under, so opening its link means switching logins in the phone browser, or reopening the tab in another Firefox container on the desktop.

- **AR.1 Tag artifact links with their account.** The daemon knows each session's account (RT-39 records which login was active at spawn and at each switch). An artifact URL (`claude.ai/code/artifact/…`, `claude.ai/artifact/…`) on screen or in the transcript's Artifact tool result is tagged with that account; the link's tooltip names it.
- **AR.2 Desktop: open in the account's Firefox container.** Settings maps each account to a container name; a click opens `ext+container:name=<container>&url=<url>` (the scheme of the "Open external links in a container" Firefox add-on), or the account's Chrome or Edge profile (`--profile-directory`).
- **AR.3 Phone: no per-account browser.** The PWA has no in-app web views with their own cookie stores, so an artifact link opens in Safari under whichever login Safari holds; another account's artifact opens through its local copy (AR.4).
- **AR.4 No login at all: the local copy.** The Artifact tool's input in the transcript names the local HTML file it published; the daemon can serve that file (PF fetch rules plus the scratchpad as an extra root, PF.14) for a read-only view. Pages that use runtime capabilities (shared data, `window.claude`) need the real link.
- **AR.5 An artifacts list.** Every artifact the sessions published, with its account, session and time, on the desktop and the phone.

## 7. Absorbing ops-tower (`OT`)

What ops-tower shows that RT must show before the owner can close it, in order of daily use:

- **OT.1 The need-you rail** → AT.3/AT.4 (waits with the permission grace, longest first; accounts running dry; Linear blocked "Questions").
- **OT.2 Agents and lanes:** the two-hour activity track (status stretches plus gate holds; `GET /api/agents/activity`'s segment shape) → the Dashboard's timeline (RT-28 DB.4); stuck flags; per-session progress lines → SV.4.
- **OT.3 Gates** → RT-55 on the `gate-state` crate (RT-64), with queue reasons ("waiting on heavy slot held by yaat `cargo test`").
- **OT.4 Accounts and pace** → RT-39 on the `claude-accounts` crate (RT-66), fleet verdict as a Fleet stream item.
- **OT.5 Board and daily briefing** → RT-56.
- **OT.6 Dispatch runs** → RT-31.
- **OT.7 Inbox badges** per repo (`%LOCALAPPDATA%\claude-inbox\repos\<repo>\*.md`) → Fleet stream.
- **OT.8 Dock bar** → RT-65.
