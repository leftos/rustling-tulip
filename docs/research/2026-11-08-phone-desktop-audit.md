PROTOCOL: 1 overlay: none (`docs/agents/explore.md` absent), read CLAUDE.md; 2 README: done (first 275 of 411 lines, glossary part; the rest unread); 3 Task Index: done via grep of `docs/architecture.md` (phone rows at 387-394, no audit row); 4 design docs: `docs/native-client.md` (chat, notifications), `docs/session-runtime.md` "Phone pushes", `docs/plans/mobile-app.md` (grep) read; `docs/wire-protocol.md` and `CHANGELOG.md` not searched; 5 source: done; 6 search: Linear list for open issues, ripgrep over `apps/pwa/src`. Tally: well over the Task Index row plus one doc (an audit spans every row).

# RT-1261 audit: phone (`apps/pwa`) against desktop (`apps/native`)

Read-only. Anchor: `X:/dev/rustling-tulip` (main checkout). Line numbers are from this checkout at the time of reading.

## Headline

1. The phone has no status vocabulary of its own: `apps/pwa/src/home-screen.ts:202` `statusWord` maps five `SessionStatus` values and ignores `background`, `is_inactive` and `is_abandoned`. `Spawning` prints as the raw string `spawning`. Desktop: `apps/native/src/status_glyph.rs` (`glyph`, the `Background` shape), `apps/native/src/dashboard_rows.rs` (`state_word`, `now_text`, `WAITING_ON_BACKGROUND`), `apps/native/src/sidebar.rs:233` (`LeafState::of`).
2. The phone never loads `ChatWork` (no `chat_work` in `apps/pwa/src` outside generated `protocol.ts`), so it has no running-work strip. Desktop: `apps/native/src/work_strip.rs`, `chat.rs` `RunningWork`.
3. Finished rule is duplicated, not divergent: `crates/daemon/src/push/trigger.rs:134` `finished` copies `apps/native/src/notify.rs:820` `finished` (same inputs, same output). The difference that matters is everything wrapped around it on the desktop: grace hold, cancel on return to Working or background, alert wording, skip-when-on-screen, sounds. The phone push fires immediately on the transition.

## Difference table

Columns: area | desktop rule (`path:symbol`, owner ruling) | phone behaviour (`path:symbol`) | class | how much a user notices | open issue | can it live once, and the approach.

### 1. Session status and running work

| # | Desktop | Phone | Class | Noticed | Issue | Once? |
|---|---|---|---|---|---|---|
| 1.1 Idle with background work | `status_glyph.rs` `glyph`: `Idle` + `background` draws the spinning Background ring; `dashboard_rows.rs:93,124` words it "Working" / "Waiting on a background task"; `docs/glossary` "Background state" (RT-84: a session whose turn ended while subagents, shells or a wakeup still run never counts as finished) | `home-screen.ts:202` `statusWord`: `idle` -> "Idle", `background` never read | Bug against owner requirement (RT-84 rule, owner report in RT-1260) | High: daily | RT-1260 (Backlog, priority 0) | Daemon: add one `status_label` (or a `display_status` enum) on `SessionSnapshot` computed beside `background`, so both clients draw one word. Interim: phone reads `background` in `statusWord` |
| 1.2 Spawning | `status_glyph.rs` `Shape::Spawning`, ring icon | `statusWord` has no case; default arm returns the raw `spawning`; `LIVE` (`home-screen.ts:14`) does include it, so Stop is offered | Gap | Low (short-lived) | none found | Same daemon label as 1.1; interim one-line `case "spawning"` |
| 1.3 Inactive (parked) and abandoned | `sidebar.rs:233` `LeafState::of`: parked wins over abandoned; each draws a tag and hover text | Row shows the underlying status word; Stop is hidden and Resume offered (`home-screen.ts:277,285`) but the row does not say "Parked" or "Abandoned" | Gap | Medium | none found | Daemon label (as 1.1) or generated enum helper |
| 1.4 Running work (subagents, shells, wakeup; monitors, gates in the agent tree) | `chat.rs` `RunningWork` from `DaemonMessage::ChatWork` (`crates/protocol/src/lib.rs:6583`), drawn by `work_strip.rs`; wording and 24-char command cut in `docs/native-client.md` "Running-work strip" (line 762); tree kinds `AgentKind` incl. `Monitor` (`protocol/src/lib.rs:1261`) | Not folded anywhere in `apps/pwa/src` | Gap | High: this is what the owner saw as "Idle" | RT-1260 names the strip as the source but does not schedule it | Daemon: a ready-worded line in the message (the strip's text produced once on the daemon) or move `RunningWork` wording into a shared crate. Wording only is cheap on the daemon; a generated TS port of `RunningWork` would copy the repaint-timer logic |
| 1.5 Monitor and MCP task as background | Not verified. `SessionSnapshot.background` doc (`protocol/src/lib.rs:932`) lists shell, subagent, one-shot wakeup only. A ripgrep of `hook_status.rs`, `pty_state.rs` for monitor/MCP found nothing tying them to the flag | Reads `background` only via the daemon push | Not verified | The owner names monitors and MCP tasks in the request | RT-1259 (In Progress) measures why `background` clears | Daemon only; both clients inherit the fix |
| 1.6 Ended / errored with attention | `needs_you.rs:104` `reason_of`: `Error`/`Stopped` with a client-held `leaf.attention` flag is a Needs You row (`Reason::Ended`, "Exited with code N") | `needsYou` (`home-model.ts:391`) lists `awaiting_input` only | Gap | Low-medium | none found | `leaf.attention` is client state; the daemon cannot hand it over without a new field. Decide first whether the phone should have it |

### 2. Notifications

| # | Desktop | Phone | Class | Noticed | Issue | Once? |
|---|---|---|---|---|---|---|
| 2.1 Finished grace | `notify.rs` `hold_finished`, `FINISHED_HOLD` 12 s, `sidebar.rs:983` `DEFAULT_FINISHED_GRACE_SECS` 5, choices 0/2/5/10/20; `docs/native-client.md:794` "an agent often starts again at once" | `crates/daemon/src/push/trigger.rs` `Tracker::observe` pushes at the transition, no hold | Bug against owner requirement (the grace is the desktop's answer to a turn end that is followed by more work) | High | RT-1259 evidences a Stop-held turn end landing 1.3 s later with `background` cleared; the grace would not cover a long gap but would cover that one | Daemon already owns the push; add the same hold there (setting on the subscription), or send both clients the same "finished" decision from one daemon-side judge (`Attention`-style) |
| 2.2 Cancel on return to work | `notify.rs:552-561` `fold_state`: a `SessionUpdated` that is not `Idle`, or has `background`, drops the held toast | None; a sent push cannot be recalled | Bug (follows 2.1) | High | RT-1259 | Daemon |
| 2.3 Finished wording | `notify.rs` `alert_title`, `WaitingAlert`/summarizer wording, label plus repo or workspace plus host (`toast_title`) | `trigger.rs` `payload` / `label`: label only (`docs/session-runtime.md:107`) | Gap | Medium | none found | Already daemon-side; the daemon holds the alerts, so feed `WaitingAlert` text into the push body |
| 2.4 Needs you source | Desktop raises on `DaemonMessage::Attention{AwaitingInput}` (`notify.rs:860`), a daemon event | Phone push judges snapshot transitions (`trigger.rs:122`, `needs_you`); rule excludes inactive and abandoned like the Needs You list | Duplicate rule, equal today | None today | none | Make both read the daemon's `Attention` event; not verified that the two fire on the same set of cases |
| 2.5 Skip when on screen | `notify.rs:982` `skips_session`, setting `notifications.skip_when` (`sidebar.rs:998` `NotifySkip`) | Per-device `hold_at_desk` only (`docs/session-runtime.md:105`, `presence.rs`) | Different rule; not wrong, two models | Medium | none | Leave; document that they differ |
| 2.6 Settings | `NotificationSettings` (`sidebar.rs:989`): `awaiting_input`, `finished`, `skip_when`, two sounds, `toast_style`, `finished_grace_secs`, `account_switches` | Subscription: Needs you, Finished, hold at desk (`notifications-view.ts`, `push-model.ts`) | Gap (no grace, no account switches) | Medium | none | Subscription fields on the daemon, additive |
| 2.7 Account switch toasts | `notify.rs:908` `raise_account_toast` | None | Gap | Low | RT-112 (PW.13 Accounts screen) is the nearest | Daemon push kind |

### 3. Prompts

| # | Desktop | Phone | Class | Noticed | Issue | Once? |
|---|---|---|---|---|---|---|
| 3.1 Card kinds and options | `docs/native-client.md:746-754`: permission (Allow once, Always allow when offered, Deny), question, review, plan, trust, Heads up; descriptions under each option | `prompt-sheet.ts:246` `buttonsFor`: all six kinds; options come from `ChatPrompt.options` so wording is the daemon's | Equal | - | - | Already shared via `ChatPrompt` |
| 3.2 Typed answers | Type something / plan "Tell <agent>" / Allow with text (only if `can_amend`) / Deny with text; blank box sends nothing | `prompt-sheet.ts:176` `resolveAction`, `permissionButtons`, `planButtons`: same set, blank refused | Equal | - | - | Rule duplicated (button set built twice: `chat_cards.rs` and `prompt-sheet.ts`). Candidate: daemon sends the button list in `ChatPrompt` |
| 3.3 Stacked cards | One current card plus ghosts behind (`native-client.md:746`) | One prompt, `chat-model.ts:88` `latestPrompt` = `screen.prompt` | Gap | Low | none | n/a |
| 3.4 Unanswerable prompt | "Open the terminal to answer this" for previews, Rewind, `/model`, `/btw` (`native-client.md:754`) | `buttonsFor` default returns `[]`; no text is shown for an empty sheet | Gap (not verified in the view) | Medium | none | Daemon sends a `needs_terminal` flag or text |
| 3.5 Heads up | Digit keys pinned over the composer, no buttons (`native-client.md:754`) | `heads_up` mapped to buttons from `options`; second step missing | Bug / gap | Medium | RT-1257 (In Progress) | `ChatPrompt` is shared; fix is the dialog parser |
| 3.6 Card answered elsewhere | Card closes with "Answered elsewhere" (`native-client.md:755`) | Sheet vanishes when prompt id changes (`prompt-sheet.ts:142` `syncSheet`); no receipt text beyond the chat's receipt row | Gap | Low | none | n/a |
| 3.7 Wrong kind (question as permission) | - | - | Daemon bug, both clients | Medium | RT-1258 (In Progress) | Daemon |
| 3.8 Home tray card | `needs_you.rs` `detail_of`/`pending_line`: richer line, plus `session_title` fallback | `home-model.ts:483` `detailOf`: own copy. "Waiting for you" vs desktop "Waiting for input" | Duplicate rule, wording differs | Low | none | Daemon-built `attention_line` on the snapshot; both read it |
| 3.9 Tray answerability | Toast asks: Allow/Deny/options plus typed text (RT desktop commits) | `home-model.ts:444` `choicesOf`: single single-select question or permission only; no typed text; plan or multi-select -> Open | Gap by design | Low | none | n/a |
| 3.10 Esc / interrupt | Esc with no card interrupts a Working turn; second Esc suppressed so Rewind is not opened (`native-client.md:755`) | `prompt-sheet.ts:85` `SHEET_KEYS` sends Esc raw via `sheetKey`; Keys tray Esc is raw too; no "after one interrupt, take Esc" guard | Gap (risk of opening Rewind) | Low-medium | none | Daemon-side guard on `ChatSend`/interrupt message |

### 4. Chat rendering

| # | Desktop | Phone | Class | Noticed | Issue | Once? |
|---|---|---|---|---|---|---|
| 4.1 Compact vs Comfortable | `chat.rs` `display_rows`; `chat_mode` in `native-ui.json`; Compact default, folds read/search/run runs by `tool_kind` | `session-screen.ts:366` one muted row per tool; no `tool_kind` use | Gap | High (long chats are noisy) | none | Port `display_rows` logic; it is pure, so generate or share. Daemon cannot fold (per-client mode). Note TS cannot import Rust, so this is a port unless RT-1152 lands |
| 4.2 Markdown | `md_blocks.rs` `SoftBreaks::Line`, `stretchtext` | `markdown.ts`, `markdown-view.ts`; `docs/architecture.md:387` says "to keep them alike" | Duplicate by design (two parsers) | Medium (stretchtext, links) | none | Two parsers is the largest long-run drift risk; share via RT-1152 |
| 4.3 Final-answer box, Worked-for, Thought-for | `native-client.md:731` | `session-screen.ts:400-419` copies `span`, "Thought for", "Worked for"; no final-answer box | Duplicate (wording) | Low | none | Daemon sends the text or leave |
| 4.4 Truncation notes | "N earlier messages" | `session-screen.ts:279` `${chat.hidden} earlier messages`; cap `DISPLAY_CAP` 500 (`chat-model.ts:7`) mirrors `ChatLog` | Duplicate (the 500 cap) | Low | none | Daemon sends the cap |
| 4.5 Streaming | Live answer: blue border, cursor, "writing - N lines" | `ChatRow.live` flag only; style not verified | Not verified | - | - | - |
| 4.6 Queued prompt, interrupt | Pending entry dimmed; message queues in CLI during a turn | `ChatRow.pending`; send during a turn: not verified | Not verified | - | - | - |
| 4.7 Summary view | Terminal / Chat / Summary switch, `LoadSummary` | Terminal / Chat toggle only (`session-screen.ts:41-43`) | Gap | Medium | none | Daemon message already exists |
| 4.8 Tail boxes, file viewer | `file_tail`, viewer tab | None | Gap | Low | RT-106 (PW.12) | - |
| 4.9 Chat unavailable | Dims Chat when the agent is not running (`sidebar::agent_up`); "This session offers no chat" | `session-screen.ts:249` falls back to terminal on `mirrorRefused` and shows the daemon's message | Equal-ish | - | - | - |
| 4.10 Plan card text | Plan card as soon as status turns waiting | Missing | Gap | Medium | RT-1232 | - |

### 5. Session list and grouping

| # | Desktop | Phone | Class | Noticed | Issue | Once? |
|---|---|---|---|---|---|---|
| 5.1 Groups | `build_containers`, `apply_container_order` | `home-model.ts:113` `homeGroups` re-implements both (comment says so); folds hosts to one "Shells"/"Folders" group | Duplicate rule (largest) | Medium if they disagree; no known disagreement | none | Daemon could send the container list already placed; today each client derives it |
| 5.2 Needs-you-on-top sort | `sidebar.rs` "Needs you on top" sort | Not present; rows in first-seen order | Gap | Medium | none | - |
| 5.3 Badges / chips | `agent_chips` (daemon-sent), tab pills, state tag | Not drawn | Gap | Low | none | Already daemon-sent |
| 5.4 Actions | Stop, Resume, Restart, Duplicate, Dismiss/Discard, Rename | Stop (two-tap), Resume (`home-actions.ts:104`, sends `DuplicateSession{same_checkout}` + `DiscardSession` follow-up) | Gap (Restart, Dismiss, Rename missing) | Medium | none | The Resume recipe is copied from the desktop (comment at `home-actions.ts:155`): put it on the daemon as one `ResumeSession` message |
| 5.5 Hidden / finished / ended sessions | Fold row "+N ended", Detached container, Recover | Ended sessions stay as `Stopped` rows | Gap | Low | none | - |

### 6. Terminal

| # | Desktop | Phone | Class | Noticed | Issue | Once? |
|---|---|---|---|---|---|---|
| 6.1 Sizing | `Resize` with `claim` (5 s of input; `native-client.md:500`) | `terminal-size.ts` `resizeFor` sends claims on input (`terminal-pane.ts:92`) | Equal | - | RT-1256 fixed | Daemon rule |
| 6.2 Links | Ctrl+click path links, spaced readings, remote `FetchFile` (`native-client.md:533-535,1017`) | `terminal-view.ts` loads `@xterm/addon-fit` only | Gap | Medium | none | - |
| 6.3 Selection / copy | `strip_box` of TUI panels, OSC 52 | xterm default; not verified | Not verified | - | - | - |
| 6.4 Scrollback | Daemon `LoadScrollback` replay | `scrollback: 5000` (`terminal-view.ts:42`) + `Scrollback` replay | Different limit; not verified | Low | none | - |
| 6.5 Reconnect | Re-attach on `ready` | `terminal-pane.ts:156` re-sends size, resets term, replays | Equal | - | - | - |

### 7. Connection states

| # | Desktop | Phone | Class | Noticed | Issue | Once? |
|---|---|---|---|---|---|---|
| 7.1 States | `connection.rs` `State` incl. `NotApproved` | `connection-machine.ts`, `connection-bar.ts` `connectionBar` | Different machines; not compared in detail | - | - | - |
| 7.2 Refused send | Text given back to the field before anything typed since, reason under the composer (`native-client.md:758`) | `composer.ts`, `prompt-sheet.ts:168` `unsent`: text given back; "Offline: not sent" (`session-screen.ts:55`) | Equal in intent; not verified end to end | - | - | - |
| 7.3 Composer note states | "Sending...", draft in prompt box, overlay open, "Can't read the prompt box" (`native-client.md:760`) | Not seen in `ComposerView` | Gap | Medium | none | Daemon already sends `ChatScreen`; the wording lives in `chat_cards.rs` -> move to the daemon |
| 7.4 Offline sends | Dropped by `HostSender` | `socket.ts:82` "Nothing is queued across a reconnect" | Equal | - | - | - |

## Fix first (in order)

1. 1.1 + 1.2 + 1.3: one daemon-computed status label on `SessionSnapshot`; phone and desktop both read it. Interim one-liners in `statusWord`. (RT-1260.)
2. 2.1 + 2.2: grace and cancel on the daemon push. Depends on what RT-1259 finds about `background` clearing.
3. 1.4: running-work line on Home and the session screen, sourced from `ChatWork`.
4. 3.5, 3.7: finish RT-1257 and RT-1258; both fixes are daemon-side and help both clients.
5. 4.1 Compact fold, 4.7 Summary: both clients' pure logic; port or share.
6. 5.4 Resume recipe on the daemon.

## Rules duplicated today, with where each copy lives

| Rule | Copy A | Copy B |
|---|---|---|
| Finished fires when | `apps/native/src/notify.rs:820` `finished` | `crates/daemon/src/push/trigger.rs:134` `finished` (comment says it mirrors A) |
| Needs you set (not inactive, not abandoned) | `apps/native/src/needs_you.rs:104` `reason_of` | `trigger.rs:147` `needs_you`; `apps/pwa/src/home-model.ts:391` `needsYou` |
| Container grouping and order | `sidebar.rs` `build_containers`, `apply_container_order` | `home-model.ts:113` `homeGroups` |
| Resume = duplicate in same checkout + discard original | `apps/native` Restart/Resume path | `home-actions.ts:104,200` |
| Allow/Deny, typed-answer button set | `apps/native/src/chat_cards.rs` | `prompt-sheet.ts:246` `buttonsFor` |
| Needs-you detail line | `needs_you.rs:119` `detail_of`, `pending_line` | `home-model.ts:483` `detailOf` |
| Duration wording and chat row labels | `chat.rs` / `chat_cards.rs` | `session-screen.ts:400-419` |
| Markdown | `md_blocks.rs` | `markdown.ts` |
| Chat display cap (500) | `ChatLog` | `chat-model.ts:7` `DISPLAY_CAP` |
| Wire types | `crates/protocol` | `apps/pwa/src/protocol.ts` (generated, test `generated_protocol_ts_is_current`): the only copy that cannot drift |

## Not checked

CHANGELOG and `docs/wire-protocol.md` owner rulings, `README.md` lines 276 to 411, `view` code (`session-view.ts`, `terminal-view.ts` DOM behaviour), CSS, the service worker and offline cache, runtime behaviour of any row marked "not verified", the Linear comment threads (only the list descriptions of RT-1257 to RT-1261 were read; RT-1257's description was not read in full), how `Attention` is emitted for 2.4, whether monitors and MCP tasks reach `background`, and tests.

## DEPENDS ON

- RT-1259 (In Progress): decides what `background` means when the screen disagrees with the `Stop` hook; rows 1.1, 1.5, 2.1 and 2.2 depend on it.
- RT-1260 (Backlog): is row 1.1; depends on RT-1259.
- RT-1258 (In Progress): daemon-side prompt kind; row 3.7.
- RT-1257 (In Progress): Heads up second step; row 3.5.
- RT-1152 (Spike, release vNext+1): if the phone shares the desktop's Rust, rows 4.1, 4.2, 5.1 and the duplicate-rules table shrink; until then fix on the daemon.

## DOC DRIFT

- `docs/architecture.md` Task Index row for the push flow and `docs/session-runtime.md:101` say the phone push follows "the rules of the desktop's toasts"; `trigger.rs` copies only the `finished` predicate, not the grace, cancel, alert wording or skip rule (`notify.rs` `hold_finished`, `fold_state`). The doc side is overstated; owner: `docs/session-runtime.md` "Phone pushes".
- `crates/protocol/src/lib.rs:932` (`background` doc) lists shell, subagent and wakeup; the owner's RT-1259 text counts monitors and MCP tasks. Not verified which side is wrong.

## TASK INDEX GAP

Row for: "Make the phone match the desktop for X (status word, notification rule, chat fold)": `docs/native-client.md` (the area's section) -> `docs/session-runtime.md` "Phone pushes" -> the desktop owner (`apps/native/src/notify.rs`, `status_glyph.rs`, `needs_you.rs`, `chat.rs`) -> `crates/daemon/src/push/trigger.rs` -> `apps/pwa/src/home-screen.ts`, `home-model.ts`, `session-screen.ts`, `prompt-sheet.ts`. This audit took roughly forty reads and searches.

## MISSING ENTRY POINT

none (`docs/architecture.md` with a Task Index and `docs/README.md` with a glossary both exist). Glossary has no entry for "Running-work strip" (defined only in `docs/native-client.md:762`) or "Home" / "Needs you tray" (the phone's names).
