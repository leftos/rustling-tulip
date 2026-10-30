# Conversation view: the chat mirror

Show a Claude session as a chat beside its terminal: the user's messages, the agent's answers, one card per tool call, quiet rows for turns nobody typed, and the session's prompts answered with buttons. The chat is a **mirror** over the live terminal session: the terminal stays the source of truth, and the desktop terminal and a phone chat of the same session can be open at once.

The same entry stream feeds two more surfaces planned here: the **running-work strip** over the composer (RT-107) and the **summary view**, one line per stretch of work, expandable into the chat (RT-111).

Linear: RT-51 (the mirror, SV.1 and SV.2), RT-107 (SV.3), RT-111 (SV.4), on the feature branch `feat/chat-mirror`. Roadmap: [off-orca.md](./off-orca.md), "Session views". Evidence: [spikes/chat-mirror-tui.md](../spikes/chat-mirror-tui.md) and its screens; mock: [canvases/chat-mirror-tui](./canvases/chat-mirror-tui/index.html).

Status engine: [screen-status.md](./screen-status.md). Summarizer: [dashboard.md](./dashboard.md). The phone's chat (RT-109, PW.10 in [mobile-app.md](./mobile-app.md)) is one more client of the same messages.

## Owner rulings

**2026-10-11.** "The chat view is a mirror over the live terminal session, not the stream-peer view switch.

The terminal stays the source of truth: entries come from tailing the session's transcript JSONL (the tailer RT-98 builds), prompts come from hook `pending_input` (RT-25), messages are typed into the PTY (bracketed paste, then Enter), and answers are typed as keys (Orca's proven rule: the option's number, never its label; the "Type something" row then the text; multi-select toggles, then Tab to Submit).

The desktop terminal and a phone chat of the same session can be open at once. `docs/plans/conversation-view.md` is redesigned around this before any CV step is briefed; the stream peer stays a later option for headless-style sessions. This wave is a feature branch, `feat/chat-mirror`. Design: `docs/plans/off-orca.md`, SV.1–SV.3."

**2026-10-21** (from the TUI spike and its mock). "Architecture: keep the mirror, add a screen engine. State from a herdr-style rule manifest over a rendered screen (regions, priorities, `not` guards), dialog options from collie-style parsers (194 pane fixtures), history from the transcript, live text from the MessageDisplay hook. Not the stdio stream peer, not a hybrid.

Composer during a terminal dialog: the typed message rides on the answer, "Yes, and tell Claude this" (Tab-amend) or "No, and tell Claude this" (deny, then send it). The chat never types a message into the PTY while a dialog is up; nothing is held by the daemon. Live answers: stream completed lines from MessageDisplay (reordered by `index`), ahead of the terminal.

Turns nobody typed (agent hand-back, finished background shell, `!` command): quiet centred system rows. The Heads up notice is pinned over the composer with its keys until answered."

**2026-10-29.** "When an AskUserQuestion or a Heads up prompt comes up, return the terminal to the main session, so the answer, or Heads up's Chat in main session draft, reaches the main agent and never a subagent being watched; especially important for the chat view." Chosen from the options put to the owner: switch both when such a prompt appears on the screen and before any chat send or answer is typed; built as a step on this branch (CV.23, CV.24).

Same day, on a side agent's note that the switch could carry a half-typed message for the subagent to the main agent: "back up the existing input, do the thing, then restore it". Chosen from the options put to the owner: the saved text goes back to the subagent, in its own view, once the prompt is answered.

Same day, after the CV.23 spike, chosen from the options put to the owner: AskUserQuestion leaves the auto-switch (its answer already reaches the main agent, and the view cannot switch while the dialog is up), so only a Heads up notice triggers it; a draft whose subagent has finished by then goes back into the main conversation's box, with a toast saying it was meant for that agent.

**2026-10-30**, after the CV.24 spike (Claude Code 2.1.314, fixtures v173–v200), chosen from the options put to the owner. The `ctrl+s` stash is one slot shared by every view, and a second `ctrl+s` silently replaces the first: the watcher stashes only when the mode line shows no `› stashed`, and when a stash is already held it does not switch and the client says "Claude is waiting in the main conversation". "Chat about this" on an AskUserQuestion answered from a subagent's view leaves the terminal on the subagent while the main agent waits: an AskUserQuestion that closes with an agent in view returns the terminal to main too, with the same draft handling as a Heads up (this replaces the AskUserQuestion part of the ruling above).

Earlier owner rulings that still apply:

- A session remembers its last view, and a new Claude session starts in the terminal view. Where the memory is kept is D-H below.
- The chat is offered for Claude sessions only. The app asks the agent's plugin for the mirror's parts (transcript parser, dialog parsers, answer keys), and only the Claude plugin supplies them.

## Terms

- **Chat entry**: one item of the chat (a user message, an answer, a thinking row, a tool card, a system row, a receipt, a turn end), carried on the wire as `ChatEntry` and upserted by id.
- **Transcript tailer**: the daemon's reader of a growing JSONL file from a byte offset, which hands each complete new line to its consumer.
- **Dialog parser**: a pure function of the plugin's that reads the rendered screen's rows and returns the dialog on it (kind, subject, numbered options, flags), in the manner of collie's parsers.
- **Answer recipe**: the key sequence the plugin gives for one answer to one dialog kind (a digit; Tab, text, Enter; toggles then submit), typed by the daemon and checked against the screen after each group of keys.
- **Composer state**: what the terminal's input area can take right now, read from the screen: ready (empty box), working (empty box, a turn running), draft (the box holds text), dialog, overlay.
- **Receipt**: the entry an answered prompt leaves in the chat, its question and the answer given.
- **Running-work strip**: the row over the composer listing the session's running subagents, background shells and the next wakeup.
- **Stretch**: a run of chat entries the summary view shows as one line: a turn, or a window of activity.

## Architecture

### One owner per fact

Every fact the chat shows has exactly one source. No fact is taken from a second source as a fallback that could disagree with the first.

| Fact | Source | Why that one |
|---|---|---|
| History: user messages, answers, tool calls and results, receipts, turn ends | the session's transcript `<conversation id>.jsonl`, tailed | the only complete and ordered record; it also holds turns taken in the terminal |
| Session state, composer state, the open dialog and its numbered options | the screen engine's grid for the session (`crates/daemon/src/screen.rs`), read by the plugin's rules and dialog parsers | dialog options exist only on the screen (spike headline 7), and several prompt ends fire no hook (headline 1) |
| That a prompt is pending, and its structured text | hook `pending_input` (RT-25) | the cheap first signal; Needs You, toasts and the phone read it; it carries the question text and labels unwrapped |
| Answer text before the message ends | the MessageDisplay hook, piece by piece, reordered by `index` | the TUI shows a message only when it is complete; the hook is about 10 s ahead (headline 3) |
| Thinking duration, Interrupted, the Heads up notice, overlays, mid-turn subagent and shell counts | the screen | the transcript has none of them |

**Orchestrator decision (not an owner ruling), D-M: four sources, one owner each.** The 2026-10-11 ruling names hook `pending_input` as the source of prompts, and the 2026-10-21 ruling names collie-style parsers as the source of dialog options. Both hold: `pending_input` says a prompt exists and gives its text for Needs You and the card's wording, and the screen's dialog parser gives the numbered options the keys address and decides whether a dialog is up at all.

A card is drawn from the parser's reading; where the hook's `PendingQuestion` text matches the parsed question, the card shows the hook's unwrapped labels and descriptions. A dialog with no hook (folder trust) still gets a card; a `pending_input` with no dialog on the screen gets none.

### Where the code lives

App code never names an agent (CLAUDE.md, "App code never names an agent"), so everything that knows Claude Code's formats lives in the Claude plugin behind `agent_api`:

- `crates/agents/claude/src/chat_entries.rs`: a transcript record to chat entries (the rebuild rules below).
- `crates/agents/claude/src/dialogs.rs`: the dialog parsers, the composer-state reader, the running-work counters, and the answer recipes.
- `crates/agents/claude/src/hooks.rs`: MessageDisplay added to the hook plan; `crates/agents/api/src/hook_vocabulary.rs` maps it.
- `crates/agents/api/src/lib.rs`: the trait methods the daemon calls, each defaulting to "unsupported", which is how a plugin without them gets no chat.

The daemon owns the plumbing:

- `crates/daemon/src/transcript_tail.rs`: the generic tailer (path, offset, line callback).
- `crates/daemon/src/chat_mirror.rs`: per watched session, the tail, the entry map, the current dialog and composer state, the live lines, and the set of loaders.
- `crates/daemon/src/server/chat.rs`: the chat messages' handlers, with their arms in `dispatch` in `crates/daemon/src/server.rs`.
- `crates/daemon/src/screen/dialogs.rs`: the glue from the session's `Screen` to the plugin's parsers, and the fixture table test over `crates/daemon/src/screen/fixtures/`.

The native client: `apps/native/src/chat.rs` (the pure model, no GPUI), `chat_view.rs` (the pane body), `chat_cards.rs` (prompt cards and the composer), `work_strip.rs` (RT-107) and `summary_view.rs` (RT-111).

**Orchestrator decision (not an owner ruling), D-N: Claude formats behind the plugin seam.** Transcript records, dialog text and key recipes are Claude Code's and change with it, so they sit in the agent crate with its fixtures, and the seam check (`tools/check-agent-names.ps1`) keeps them there. The daemon keeps only what any agent's mirror would need.

### The transcript tailer

The tailer follows one file from a byte offset. It wakes on a file watcher event and on a 1.5 s size poll (a watcher alone misses appends on some file systems), reads only the new bytes, holds a partial last line until its newline arrives, and starts over from 0 when the file shrinks or is replaced.

It parks while no client is connected (`Hub.client_count` 0, as `git_watch.rs` does) and catches up on the next connect. A session is tailed only while at least one connection has loaded its chat or summary.

The mirror follows the session record's conversation id: `SessionStart` with `clear`, `resume` or `compact` updates it (hook-status HS.7), and the mirror then tails the new file. The path comes from the plugin (Claude: `<claude home>/projects/<encoded cwd>/<id>.jsonl`, `crates/agents/claude/src/transcripts.rs`, whose `project_dir` becomes public).

**Orchestrator decision (not an owner ruling), D-R: the tailer is its own module.** `transcript_tail.rs` knows nothing of chat entries, so RT-98's file-path index and RT-30's subagent watcher read their files through the same tailer (D-C).

### Rebuilding the chat from the transcript

The rules, from a read of how Claude Desktop rebuilds a session and from the spike:

- The plugin turns one line into entries in file order; the mirror keeps entries in file order too, in a reply and in appends alike (`ChatAppend` carries no position), so an attachment written out of order shows where the file put it. The plugin's `Agent::chat_line_facts(line)` reads a line's `promptId` and whether it is a user prompt (`TranscriptLineFacts`). **Orchestrator decision (not an owner ruling):** placing by `timestamp` was dropped after the CV.7 review, so a live chat and a reload always agree on order.
- `user` records with text are user messages. `attachment` records of type `queued_command` are user messages typed mid-turn; those with `commandMode: "task-notification"` are skipped, since the notification arrives again as its own record.
- A `user` record with `origin.kind: "task-notification"` is a turn nobody typed: a **system row** "Background command `<summary>` completed" or "Agent `<description>` finished" from its `<status>` and `<summary>`. `<bash-input>` and `<bash-stdout>` records are a `!` command: one system row with the command and its output folded.
- `<pasted_content>` is unwrapped to the pasted text. Slash-command output (`system` `local_command`) has its SGR escapes stripped and shows as a system row.
- `assistant` text blocks are answers; `tool_use` blocks are tool cards, and the `tool_result` with the same `tool_use_id` attaches to its card. An AskUserQuestion result's `toolUseResult.answers` becomes a **receipt**; a plan approval becomes a receipt too.
- A signed, empty `thinking` block is a thinking row at its place; its duration is the record's own `thinkingDurationMs`, rounded to seconds (D-L). `redacted_thinking` and unsigned thinking are dropped.
- A task notification's row is its own `<summary>` sentence, shown as written with its first quoted name as code; a monitor event's notification is a row the same way. A `system` `turn_duration` record is the turn's end, with its `durationMs`. An interrupt (`[Request interrupted by user]`, with or without `for tool use`) is its own record and a system row "Interrupted".
- Compact summaries (`isCompactSummary`), `attribution-snapshot`, `file-history-*`, `mode`, `api-request*` and the other non-message records are dropped; a `compact_boundary` becomes the system row "Conversation compacted".
- A permission amend (the text block beside a tool_result) is a user message; a deny is the tool card's error result plus the system row "Interrupted".
- Subagent records live in `<id>/subagents/agent-<id>.jsonl` and are not merged into the chat; an `Agent` tool card shows the subagent's description and status, and opening its stream is RT-30's.
- Text is capped per entry: tool inputs 4 KiB, tool results 8 KiB, answers uncapped. A `LoadChat` reply carries the newest 500 entries, with `hidden` counting the older ones.

Desktop also reads `~/.claude/sessions/<pid>.json` to know whether a CLI is still running. The mirror does not need it: the daemon owns the PTY, the tracer and the hooks, and knows the session's liveness and conversation id from its own record.

### The screen engine: dialogs and the composer

The screen engine (`screen.rs`, `screen/rules.rs`) already reads a session's status from the rendered grid with the plugin's rules, behind hysteresis. The mirror adds dialog parsers and the composer reader over the same grid, called by `chat_mirror.rs` after each debounced read the status watcher makes; there is no second grid.

The v1 dialog set (D-K):

| Dialog | Parsed | Card |
|---|---|---|
| Permission | kind (`Bash command`, `Create file`, `Edit file`), subject, content box, numbered options, `Tab to amend` | Allow once, Always allow (when offered), Deny, and the two ride-on answers |
| AskUserQuestion, single and multi-select | tab bar (questions, which are answered), question, options with descriptions, `Type something`, `Next` | one question at a time with Back, Skip and Next |
| Review your answers | the answers listed | Submit, Cancel |
| Plan approval | the plan box, the three options | Approve (auto), Approve (manual edits), Tell Claude what to change |
| Folder trust | the two options | Trust this folder, Exit |
| Heads up notice | the notice, its options (`1: Learn more   2: Knew this already   0: Dismiss`, and the expanded set) | pinned over the composer with its keys until answered |
| AskUserQuestion with previews, Rewind, detailed transcript, `/model`, `/btw`, the slash popup | recognised only | composer disabled with "Open the terminal to answer this" |

**The agent in view.** Claude Code's terminal can show one subagent's transcript in place of the main conversation (opened from the agents panel under the mode line), and while it does, the prompt's next Enter goes to that subagent. There is one input box for every view: a draft rides along on a switch (spike, "The agent in view (CV.23)").

The plugin reads from the screen whether an agent's transcript is in view (the rule over the box ends with the agent's description, the empty box reads `Message @<agent type>…`, the panel's filled dot is on the agent's row) and which of the panel or the box holds the keys (the hint line alone tells).

It gives two recipes, each a key at a time with a screen check after every key: **return to main** (Down until the hint reads `↑/↓ to select`, at most three, then Enter on `main`, then Up until the box has the keys) and **reopen an agent** (Down to the panel, Down to the row whose description matches, Enter, then Up). Neither ever sends Esc (it interrupts the subagent's running tool), Left (it opens "Background this session?"), Enter on an empty box (it opens the Background tasks list) or an Up past the box (it recalls the view's prompt history).

Per the 2026-10-29 rulings, the status watcher types **return to main** for every Claude session, chat open or not, as soon as a reading shows an agent in view with a Heads up notice up, once per notice, so the notice's Chat in main session draft fills the main conversation's box. An AskUserQuestion triggers nothing: its answer reaches the main agent from either view.

`ChatSend` and `ChatAnswer` type **return to main** first whenever an agent is in view, and go on only once a reading shows the main conversation; a reading that still shows the agent stops them with an `Error`.

A draft in the subagent's box is never carried across. Before the switch the watcher saves it with `ctrl+s` (Claude Code's prompt stash, which empties the box and keeps a pasted-text placeholder whole); once the notice is gone, it reopens that subagent's view and presses `ctrl+s` on the empty box to put the draft back.

When the subagent's row is gone by then (it finished), the draft goes back into the main conversation's box instead, and the daemon sends the loaders and the session's clients a notice the client shows as a toast: the draft was meant for that agent, which has finished.

A box holding the notice's own Chat in main session draft (key 2 pressed before the switch) is carried across unsaved, since that draft is for the main conversation.

The composer reader returns the composer state: **ready** (prompt box empty, no dialog), **working** (spinner, box empty), **draft** (the box holds text: the user's own draft, a queued message, or a Heads up "Chat in main session" draft), **dialog**, or **overlay**.

### Live answers from MessageDisplay

MessageDisplay is added to the hooks the Claude plugin installs. The hook body carries `session_id`, `transcript_path`, `cwd`, `prompt_id`, `hook_event_name`, `turn_id`, `message_id`, `index`, `final` and `delta`, and no `agent_id`. The plugin turns it into an `agent_api::LiveLine { text, index, message_key, prompt_key }` (`text` from `delta`, `message_key` from `message_id`, `prompt_key` from `prompt_id`; a body without a string `delta` or a non-negative whole `index` gives no line, and `final` is not carried). The daemon delivers it to the session's `hook_status::LiveLines` broadcast, which has no status effect, and `chat_mirror.rs` keeps an `Answer { live: true }` entry per message, placing each piece by `index` and broadcasting at most every 100 ms.

Measured with Claude Code 2.1.314 in an interactive session (a ConPTY, `--settings` holding only this hook): a 40-line answer came as 40 calls, one per completed line, `index` 0 to 39 and `final: true` only on the last, 0.16 to 0.55 s apart; the first arrived 11.9 s before the transcript's `assistant` record and before the TUI drew the message (it draws a whole message in one frame when the record is written), the last at the same moment. A piece always ends at a line boundary but may hold several lines (one call carried 11), and a short message is a single `final: true` call, the same shape the print-mode capture showed. Each API message has its own `message_id` and its `index` restarts at 0. The calls are async: they arrived in order in that run, and the mirror still orders by `index` and assumes no count. The lead is the generation time left in a message, so live text matters for long answers and adds nothing to short ones. The body also carries `scratchpad_dir`, which the plugin ignores.

No id in the body appears on the transcript's `assistant` record (`message.id`, `requestId`, `uuid`); `prompt_id` appears as `promptId` on the prompt's `user` record. Orchestrator decision (not an owner ruling): when the transcript has the answer, the mirror matches the live line's `prompt_key` to that `promptId` (read by `chat_line_facts`) and takes the assistant text that follows it, telling several assistant messages in one turn apart by `message_key` and arrival order. That replaces the live entry and its `live` flag clears. A Claude Code without the hook, or a session spawned before it, shows the answer when the transcript gets it (D-J).

### Typing a message

`ChatSend` types the text into the PTY as one bracketed paste (`ESC [200~ … ESC [201~`) followed by Enter, through `pty.rs` `write_input`. The daemon reads the composer state first and refuses the send with an `Error` naming the reason unless the state is ready or working:

- **dialog**: the client offers the ride-on answers instead (below); the chat never types a message into the PTY while a dialog is up.
- **draft**: "The terminal's input box holds text"; the user clears it in the terminal or sends it there.
- **overlay**: "Open the terminal to close this first."

While a turn runs, the send goes straight to the CLI's own queue (D-I). The chat shows the message dimmed as `pending` until the transcript's `queued_command` attachment or `user` record for it arrives. A paste of 30 lines or more is shown in the terminal as `[Pasted text #1 +N lines]`; the transcript carries it whole and the chat shows it whole.

**Orchestrator decision (not an owner ruling), D-O: refuse, never queue, in the daemon.** The 2026-10-21 ruling says nothing is held by the daemon. A send the screen cannot take now is refused at once, so the user sees why, and the text stays in the chat's composer to send again.

### Answering a prompt

`ChatAnswer` names the prompt it answers (its id, minted per dialog reading) and the answer. The daemon refuses it when the current dialog is not that prompt (answered elsewhere, or changed), then types the plugin's recipe:

| Answer | Keys |
|---|---|
| Permission: Allow once, Always allow, No | the option's digit; no Enter |
| Permission: Yes, and tell Claude this | Tab, the text, Enter |
| Permission: No, and tell Claude this | the No option's digit; once the composer state reads ready or working, the text as a normal `ChatSend` |
| AskUserQuestion, single choice | the option's digit (it moves to the next question) |
| AskUserQuestion, multi-select | each option's digit toggles it; then submit (open question 1) |
| AskUserQuestion, Type something | Down until the cursor is on that row, the text, Enter |
| Back, Skip | Left, Right on the tab bar |
| Review | `1` submits, `2` cancels |
| Plan approval | `1` or `2`; `3`, the text, Enter |
| Folder trust | Up or Down from the cursor's row to the chosen row, then Enter (digits do nothing; builds start the cursor on different rows) |
| Heads up | the bare digit |

After each group of keys the daemon waits for the next screen reading and checks it moved as the recipe expects (the next question, the dialog gone); a reading that does not match stops the recipe and answers `Error` with the screen's state, and the card shows what the terminal now shows.

No recipe ever sends Esc Esc (it opens Rewind on an empty box) or ctrl+c. The daemon keeps no answer queue: a recipe runs to its end or stops.

**Orchestrator decision (not an owner ruling), D-P: recipes are data, and screen-checked.** The keys that work were measured per dialog (spike, "Answering with keys") and differ from the hook's suggestions; keeping them as a table in the plugin, pinned by fixture tests, makes a Claude Code change a failed check and a refused answer rather than a wrong option chosen.

### Several clients and the terminal at once

The chat never resizes the PTY: its size is set by the panes that show the terminal (CLAUDE.md, `Resize` claims). A phone chat and the desktop terminal of one session run together; a key typed in the terminal and an answer from the chat meet on the same screen, and whichever lands first moves the dialog, so the other is refused as answered elsewhere.

Every connection that loads a session's chat shares one tail and one entry map; the last `UnloadChat` (or the connection's end) stops the tail.

**Orchestrator decision (not an owner ruling), D-S: one grid, one tail, refcounted loaders.** The status watcher's grid is the screen the owner sees, so the mirror reads it rather than building another.

### Shell-hosted agents, restarts and ends

- **Agent in shell** (`agent_in_shell`): the same path. While the agent runs, the chat is live; once it has ended (`AgentExited`), the chat keeps the last conversation, read-only, with the composer disabled ("Claude is not running in this shell"), until the next `SessionStart` names a conversation. Detecting that the agent is up again shares RT-438's work.
- **Daemon restart**: the tracer keeps the session; the reattached session's watcher rebuilds its grid from the replay, and a client's next `LoadChat` re-reads the transcript. A live line held only in memory is lost and the transcript fills it in.
- **`/clear`**: the conversation id changes (HS.7) and the chat starts over on the new file, with the system row "Conversation cleared".
- **Billing**: the chat runs no second `claude` process, so [gui-mode-billing.md](../spikes/gui-mode-billing.md) does not apply to it.

## Wire protocol

Additive only; `protocol-version.json` stays at its current version. Every new tagged enum carries `#[serde(other)] Unknown`.

Client to daemon:

- `LoadChat { session_id, request_id }` and `UnloadChat { session_id }`.
- `ChatSend { session_id, text, request_id }`.
- `ChatAnswer { session_id, prompt_id, answer: ChatAnswerKind, request_id }`, `ChatAnswerKind` (tag `kind`): `Option { number }`, `Toggle { number }`, `Submit`, `TypeSomething { text }`, `AllowWithText { text }`, `DenyWithText { text }`, `Back`, `Skip`, `Unknown`.

Daemon to client:

- `Chat { session_id, entries, hidden, screen: ChatScreen, request_id }`, the reply to `LoadChat`.
- `ChatAppend { session_id, entries, removed }` to loaders only; an entry whose id is known replaces it, and `removed` (ids, serde default) lists entries that leave the chat, so one append carries a transcript answer and the live id it replaces together.
- An unsolicited `Chat` (`request_id: None`) is a full reset, sent for a new conversation id (opening with the system row "Conversation cleared", id `cleared:<id>`, only when both the old and the new conversation are known) and for a transcript that restarted. `LoadChat` answers the newest 500 entries (`CHAT_REPLY_CAP` in `chat_mirror.rs`) with `hidden` counting the rest.
- `ChatScreenChanged { session_id, screen }` to loaders only, when the dialog or composer state changes.
- `ChatScreen { composer: ComposerState, prompt: Option<ChatPrompt> }`. `ComposerState`: `Ready`, `Working`, `Draft`, `Dialog`, `Overlay { name }`, `Unknown`.
- `ChatPrompt { id, kind: ChatPromptKind, subject, body, options: Vec<ChatOption>, questions: Vec<ChatQuestionTab>, can_amend, always_allow_tip }`, `ChatPromptKind`: `Permission`, `Question`, `Review`, `Plan`, `Trust`, `HeadsUp`, `Unknown`; `ChatOption { number, label, description, selected }`.
- `ChatEntry` (tag `kind`): `User { id, text, pending }`, `Answer { id, text, live }`, `Thinking { id, secs }`, `Tool { id, name, summary, input, result: Option<ToolOutcome>, parent_id }`, `System { id, text }`, `Receipt { id, question, answer }`, `TurnEnd { id, duration_ms }`, `Unknown`.

Refusals answer `Error` carrying the request's `request_id`. RT-107 adds `ChatWork` and RT-111 the summary messages (below). Protocol tests: each message round-trips, each new enum decodes an unknown tag to `Unknown`.

## Native client

- **Model**, `apps/native/src/chat.rs`, no GPUI, depending only on `protocol`: entries by id with upsert and the display cap, card expansion, tool one-liners (a path, a command, a pattern), the answer builders, and the stacking of cards when a session has more than one prompt in a row. It moves into the shared client core when the phone takes it over.
- **Pane body**, `chat_view.rs`: a scrolling chat that follows the bottom until the user scrolls up; user messages right-aligned; answers as Markdown through `md_blocks.rs` with code through `syntax.rs`; live answers growing line by line; thinking as one muted line (`Thought for 14s`); tool cards with name, summary and a status mark, expanding to input and result; system rows centred and muted; a turn's duration under its last answer.
- **Cards**, `chat_cards.rs`: one card for the current prompt, with any others stacked behind it as ghosts. Digits answer only from an empty composer and only after a 400 ms grace from the card's appearance; Esc denies, Ctrl+Enter allows once, Ctrl+Shift+Enter always allows.

  One "Always allow" label, with a tooltip naming the rule and where it is saved (the dialog's own option text, and the hook suggestion's `destination` when it has one). AskUserQuestion shows one question at a time with Back, Skip and Next. A submitted card stays, busy, until the screen shows the dialog gone, then collapses to its receipt; a card answered in the terminal or on another device closes with "Answered elsewhere".
- **Composer**: `TextInput::multi_line`, Ctrl+Enter sends, during a turn too. While a dialog is up, Send becomes two choices on the card, "Yes, and tell Claude this" and "No, and tell Claude this", and the text rides on the answer. With the composer state `Draft` or `Overlay` the composer shows why it cannot send. The Heads up notice is pinned over the composer with its keys until answered.
- **Choosing the view**: each pane shows Terminal or Chat (and Summary, RT-111), chosen in the pane header, the pane menu and a keybinding in `keys.rs`; the choice is the pane's, and the session is unchanged. The last view chosen for a session is kept per session id in `native-ui.json` and is the view the next pane showing it opens in; a new session opens in Terminal. A pop-out pane keeps its view.

  A pane that shows Chat sends `LoadChat` when it appears and after a reconnect, and the last such pane for a session sends `UnloadChat`.

**Orchestrator decision (not an owner ruling), D-Q: Markdown through the existing viewer model.** `apps/native/src/md_blocks.rs` (pulldown-cmark, already a dependency) is the Markdown viewer's block model; answers render through it rather than a new splitter.

## Running-work strip (RT-107)

Over the composer in the chat and summary views: "2 subagents · 1 background shell (`cargo test`, 3m) · wakeup in 4m". It is the background state (AT.1) made visible, so a quiet chat is never taken for an idle session.

- **At a turn's end**: the `Stop` payload's `background_tasks` (each `{id, type, status, description, command}` for a shell, `{id, type, description, agent_type}` for a subagent) and its one-shot `session_crons` (the wakeup's fire time) are kept on the session record, beyond the `background` flag RT-84 already derives from them (`hook_vocabulary.rs`, [spikes/background-tasks.md](../spikes/background-tasks.md)). A task's age counts from the first `Stop` that listed it.
- **Mid-turn**: the plugin's counters read the screen: `● Running N agents…`, the agents panel rows under the mode line, and the mode line's `· N shell`. These are counts and labels only; a count read from the screen says so in its tooltip.
- **Ends**: a task leaves the strip when a later `Stop` no longer lists it, or when its `<task-notification>` record arrives in the transcript; a wakeup leaves when its time passes and a turn starts.
- **Delivery**: `ChatWork { session_id, subagents, shells, wakeup_at }` to chat and summary loaders only, on load and on each change (D-E).
- **Taps**: a subagent item opens its stream once RT-30 lands; until then it is not tappable. A shell item shows its full command in a tooltip.

## Summary view (RT-111)

A third view of a session beside terminal and chat: one line per stretch of work, the open question pinned at the top, every line expandable into the chat entries of that stretch, and the running-work strip over its composer. The phone opens sessions here by default.

- **Stretches**: one per turn, split into windows of activity for a long turn (ops-tower's progress lines: the last 5 minutes, 5–15, 15–30). Each stretch keeps the range of chat entry ids it covers, so expanding it shows exactly those entries.
- **Lines**: written by the summarizer the Dashboard uses ([dashboard.md](./dashboard.md), RT-28's summarizer service, its `headline`), through its queue, cache and spend cap. With no provider key, a failure or the cap reached, each line is a template: the turn's first user line and its duration and tool count (D-G).
- **The pinned question**: the current `ChatPrompt`, or the summarizer's `needs_answer` text at a stop, answered from the same card as in the chat.
- **Wire**: `LoadSummary { session_id, request_id }`, `UnloadSummary`, `Summary { session_id, lines, request_id }`, `SummaryAppend { session_id, lines }`, `SummaryLine { id, from_entry, to_entry, started_at, ended_at, text, source }`, reusing RT-444's summary types where they already carry these fields.

## Decisions

Each recommendation of the code map behind this design, taken by the orchestrator where no owner ruling decides it.

- **D-A Answer keys count as the owner's.** Orchestrator decision (not an owner ruling): keys typed by `ChatAnswer` clear `AwaitingInput` and `pending_input` the way an owner key does, but only once the screen reading shows the dialog gone; until then the status watcher decides. Rationale: the card closes at once on a real answer, and a recipe stopped half-way leaves the prompt pending, which is the truth. Tests cover both paths.
- **D-B Commits land on the feature branch, one PR at the end.** Orchestrator decision (not an owner ruling): each step's commit lands straight on `feat/chat-mirror`, merged by the orchestrator; one PR to `main` after the live e2e, gated by a code review and a hand test of every prompt kind against a real `claude`. Rationale: the owner put the wave on a feature branch; per-step PRs would add review overhead on a branch nobody else ships from.
- **D-C One tailer, built here.** Orchestrator decision (not an owner ruling): CV.2 builds the transcript tailer as a generic module, and RT-98's file-path index becomes a consumer of it later; RT-98 moves into this wave only if the owner wants the index shipped now. Rationale: the 2026-10-11 ruling names RT-98's tailer, and there is still one tailer; building it here unblocks the chat without pulling in the index.
- **D-D The strip reads hooks already installed, and the screen.** Orchestrator decision (not an owner ruling): no `SubagentStart` / `SubagentStop` hooks for the strip; it uses the `Stop` payload's lists and the screen's counters. Rationale: those hooks belong to AT.1 and RT-30; the `Stop` lists already name each task and the wakeup, and the screen covers the turn in between.
- **D-E The strip travels to loaders only.** Orchestrator decision (not an owner ruling): `ChatWork` is a message to chat and summary loaders, not a `SessionSnapshot` field. Rationale: the snapshot is broadcast to every client and RT-444 is changing it now; the strip is needed only where a chat or summary is open.
- **D-F One entry type with subagent streams.** Orchestrator decision (not an owner ruling): `ChatEntry` is defined here and RT-30's subagent view (SS.3) uses it in place of its planned `TranscriptEntry`. Rationale: one parser of Claude's message shape, not two drifting apart.
- **D-G Summary lines without a key are templates.** Orchestrator decision (not an owner ruling): with no provider key the summary view still ships, one template line per stretch from its first user line and its stats; model lines replace them once the summarizer runs. Rationale: RT-111 does not wait on RT-26 and RT-27.
- **D-H The view is the pane's; the memory is the client's.** Orchestrator decision (not an owner ruling): each pane picks its view, and a session's last view is kept per session id in `native-ui.json` (the phone keeps its own), not on the daemon's session. Rationale: the 2026-10-11 ruling has the terminal and a chat of one session open at once, so a single view on the session cannot describe it; the earlier ruling that a session remembers its last view is kept, on the client.
- **D-I A mid-turn send goes straight to the CLI.** Orchestrator decision (not an owner ruling): a message sent while a turn runs is typed into the PTY at once and waits in the TUI's own queue (`Press up to edit queued messages`, `ctrl+enter to send now`).

  Claude Desktop does the opposite: it holds a plain send until `result` and offers "promote" and "steer now". Rationale: the 2026-10-21 ruling says nothing is held by the daemon, and the TUI's queue already gives the user edit and send-now in the terminal.
- **D-J MessageDisplay live text is in the first cut.** Orchestrator decision (not an owner ruling): live lines land with the mirror core, and the chat still works without them (the transcript alone). Rationale: the 2026-10-21 ruling asks for live answers; a Claude Code without the hook must still render.
- **D-K The v1 dialog set.** Orchestrator decision (not an owner ruling): cards for permission, AskUserQuestion (single choice, multi-select, Type something, review), plan approval, folder trust and the Heads up notice; AskUserQuestion with previews, Rewind, the detailed transcript, `/model` and `/btw` disable the composer with "Open the terminal to answer this".

  Rationale: the cards cover every prompt the spike saw in normal work, and the rest are rare or need a preview pane a phone cannot draw well.
- **D-L Thinking rows come from the transcript.** Orchestrator decision (not an owner ruling): a thinking row sits where the transcript's signed, empty `thinking` block is, and shows the record's `thinkingDurationMs` rounded to seconds, or none when the record lacks it. Rationale: every signed thinking record carries its duration, so the transcript gives both the place and the time and the screen is not needed.

Further decisions made in this redesign (D-M to D-S) sit beside the parts they decide, above.

## Alternatives not taken

- **Claude Desktop's design**: it does not mirror a live TUI. It runs its own pinned CLI over pipes (`--input-format stream-json --output-format stream-json --permission-prompt-tool stdio --replay-user-messages --include-partial-messages`), answers `can_use_tool` control requests, and imports a terminal-started session only after its CLI exits. The owner chose the mirror over it (2026-10-21 ruling); recorded here as context.

## Later option: the stream peer

For headless-style sessions only, a later design may drive a `claude --print --input-format stream-json --output-format stream-json` child (the **stream peer**) the daemon writes JSON lines to, with permission prompts over the stdio control protocol. It is not planned; the notes below keep what was learnt for it.

- Documented flags (Claude Code 2.1.x CLI reference): `--input-format stream-json` is for print mode; `--replay-user-messages` re-emits stdin messages; `--include-partial-messages` streams deltas; with stream-json input a message queued when a turn ends starts a new turn; `--permission-prompts host` sends prompts to the SDK host. https://code.claude.com/docs/en/cli-reference, https://code.claude.com/docs/en/headless
- The SDK's `control_request` / `control_response` pair carries permission checks by `request_id`; allow-always echoes a `localSettings` suggestion in `updatedPermissions`; AskUserQuestion answers as `updatedInput: {questions, answers}`. https://code.claude.com/docs/en/agent-sdk/typescript, https://code.claude.com/docs/en/agent-sdk/user-input
- Desktop's wire shapes, read from its build: `can_use_tool` fields, a plain deny sets `interrupt: true` and a deny with feedback does not, `request_user_dialog`, `control_cancel_request`, re-sent `pending_permission_requests`, `interrupt{cancel_queued:true}`, `priority: "next"|"now"`, `cancel_async_message`; SDK-mode AskUserQuestion needs `CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL=true`.
- A stream peer would need its own spike (the exact control JSON on Windows), a tracer pipe mode to survive daemon restarts, and its own argv builder in the plugin. None of it is on the chat mirror's path.

## Open questions

1. **Submitting a multi-select question.** The 2026-10-11 ruling gives Orca's rule "multi-select toggles, then Tab to Submit". The spike, on Claude Code 2.1.307, saw digits toggle without moving the cursor, Enter on the `Next` row move on, and Tab move the cursor down one row.

   (a) Recommended: follow the measured keys (move to `Next`, Enter), checked against the screen, and keep the ruling's intent (toggles, then submit). Worst case: an Orca-era build where Tab submits needs a second recipe row. (b) Tab, as ruled. Worst case: on today's build Tab moves the cursor and the screen check stops the recipe every time.

## Steps

Each step is one brief on `feat/chat-mirror`. Steps that edit `crates/protocol/src/lib.rs`, `crates/daemon/src/server.rs` or `apps/native/src/lib.rs` and `grid_view.rs` never run at once.

### The mirror (RT-51)

- [x] **CV.1 Add the chat protocol types and messages.** `crates/protocol/src/lib.rs`: `ChatEntry`, `ToolOutcome`, `ChatPrompt`, `ChatPromptKind`, `ChatOption`, `ChatQuestionTab`, `ChatScreen`, `ComposerState`, `ChatAnswerKind`, the four client messages and three daemon messages.

  Arms in `crates/daemon/src/server.rs` `dispatch` (answering `Error` with the `request_id`) and in `apps/native/src/lib.rs` `on_message` (ignored), each only if its match is exhaustive. Proof: round-trip tests and an `Unknown` test per new enum. Depends on nothing.
- [x] **CV.2 Add the transcript tailer.** New `crates/daemon/src/transcript_tail.rs`, its `mod` line in `crates/daemon/src/main.rs`: path, offset, watcher plus 1.5 s size poll, partial last line held, restart on shrink or replace, park and catch up. Proof: unit tests on temp files: appended lines arrive once each, a half-written line waits for its newline, a truncated file starts over, a missing file yields nothing until it appears. Depends on nothing.
- [x] **CV.3 Turn transcript records into chat entries.** New `crates/agents/claude/src/chat_entries.rs`, `crates/agents/claude/src/lib.rs`, `crates/agents/claude/src/transcripts.rs` (`project_dir` public), `crates/agents/api/src/lib.rs` (the trait methods for the transcript path and one record to entries, defaulting to unsupported), redacted fixture JSONL under `crates/agents/claude/tests/fixtures/transcripts/`.

  Proof: tests per rebuild rule: tool results attach by `tool_use_id`; `queued_command` is a user message and its task-notification form is skipped; a task notification and a `!` command are system rows; compact summaries, `attribution-snapshot`, `redacted_thinking` and unsigned thinking are dropped; `<pasted_content>` unwraps; an AskUserQuestion result is a receipt; caps apply. Depends on CV.1.
- [x] **CV.4 Parse permission, plan, trust and Heads up dialogs and the composer state, with their answer recipes.** New `crates/agents/claude/src/dialogs.rs`, `crates/agents/claude/src/lib.rs`, `crates/agents/api/src/lib.rs` (dialog, composer and recipe methods), new `crates/daemon/src/screen/dialogs.rs` (glue and the fixture table test), `crates/daemon/src/screen.rs` (`mod` line), fixtures added to `crates/daemon/src/screen/fixtures/` from the spike's screens.

  Proof: a fixture-to-reading table (kind, options, composer state, overlay) over every fixture, and a recipe table test per answer. Depends on CV.1.
- [x] **CV.5 Parse AskUserQuestion dialogs with their answer recipes.** `crates/agents/claude/src/dialogs.rs`, `crates/daemon/src/screen/dialogs.rs`, fixtures for single choice, multi-select, Type something, review, previews and the tab bar from the spike (`v31`–`v43`) and collie's panes. Proof: the table test extended; previews read as an overlay; a wrapped cui card parses as vanilla does. Depends on CV.4.
- [x] **CV.23 Capture the terminal with a subagent's transcript in view.** Spike against a real `claude` (no code): open a running subagent from the tasks list, capture the screen as fixtures under `crates/daemon/src/screen/fixtures/` (idle, with a Heads up notice up, with an AskUserQuestion dialog up, with a Chat in main session draft), and with a draft typed to the subagent. Measure the keys that return to the main conversation from each, and the keys that reopen that same subagent's view.

  Measure the draft too: whether each view keeps its own draft across a switch, whether the prompt stash keeps a pasted-text placeholder and an image whole, and how to clear the box and type a saved draft back without sending it.

  Proof: the fixtures, and a note in `docs/spikes/chat-mirror-tui.md` with the measured keys, the draft findings and what each screen shows. Depends on nothing.
- [ ] **CV.24 Return the terminal to the main conversation for a Heads up.** First measure what CV.23 left open, against a real `claude`, adding fixtures: the built-in Heads up in a subagent's view (CV.23 saw only a copy of its band), a second `ctrl+s` while a draft is already stashed, an image through `ctrl+s`, and AskUserQuestion's Chat about this from a subagent's view. A finding that breaks the design above goes to the owner before any code.

  Then `crates/agents/claude/src/dialogs.rs` (the agent-in-view and key-holder reader, the two recipes as key-and-check steps), `crates/agents/api/src/lib.rs` (their trait methods, defaulting to unsupported), `crates/daemon/src/screen/dialogs.rs` (the fixture table over v160–v172 and the new fixtures), `crates/daemon/src/pty_state.rs` (the watcher's switch, stash and restore).

  Also `crates/protocol/src/lib.rs` (an additive daemon message for the draft-returned-to-main notice, with its `#[serde(other)]`-safe decode test), `crates/daemon/src/server.rs` and `apps/native/src/lib.rs` (sending it, and the client's toast).

  Proof: fixture tests: v162–v172 read agent-in-view or not as captured, and the key holder as the hint shows; recipe tests: each step's key waits for its check, Down repeats at most three times, and no recipe holds Esc, Left, Enter on an empty box or an extra Up.

  Watcher tests against a recording PTY: a Heads up with an agent in view types **return to main** once per notice, never with main in view, and never for an AskUserQuestion; a draft is stashed before the switch and restored with `ctrl+s` in the reopened view after the notice goes; a finished agent's draft is restored in the main box and the notice is sent; a Chat in main session draft is carried unstashed. Depends on CV.5, CV.23.
- [x] **CV.6 Carry MessageDisplay lines to the daemon.** `crates/agents/claude/src/hooks.rs` (the event in the hook plan), `crates/agents/api/src/hook_vocabulary.rs` (its report: a `LiveLine` with text, `index`, `message_key` and `prompt_key`), `crates/daemon/src/hook_status.rs` and `crates/daemon/src/server/hooks.rs` (`hook_status::deliver` and `LiveLines`, a per-session broadcast of live lines with no status effect), a captured payload under `crates/agents/claude/tests/fixtures/hooks/`.

  Proof: first record one real MessageDisplay body into the fixture; mapping tests over it; out-of-order lines keep their `index`. Depends on CV.1.
- [x] **CV.7 Serve a session's chat from the transcript and the screen.**

  New `crates/daemon/src/chat_mirror.rs`, new `crates/daemon/src/server/chat.rs`, `crates/daemon/src/server.rs` (dispatch arms), `crates/daemon/src/pty_state.rs` (hand each screen reading to the mirror), `crates/daemon/src/main.rs` (`mod` line): `LoadChat` / `UnloadChat`, the newest 500 (`CHAT_REPLY_CAP`) with `hidden`, `ChatAppend` (with `removed`) and `ChatScreenChanged` to loaders, live lines merged by `index` and replaced by the transcript's answer (matched through `prompt_key` and the user record's `promptId`, messages in one turn told apart by `message_key` and arrival order; the live answers of earlier prompts end at the next prompt), the tail following the conversation id. A replacement goes to a loader only for an entry at or after the first one its last `Chat` carried.

  Proof: server tests with a temp `CLAUDE_CONFIG_DIR` and scripted transcript writes: appends reach loaders only; the reply caps and echoes `request_id`; a missing transcript yields none; the last unload stops the tail; a `/clear` switches files; a fixture screen yields its `ChatScreen`. Depends on CV.2, CV.3, CV.4, CV.6.
- [ ] **CV.8 Type chat messages into the PTY.** `crates/daemon/src/server/chat.rs` (`ChatSend`, gated by the composer state), `crates/daemon/src/inject.rs` (a bracketed paste then Enter helper), `crates/daemon/src/chat_mirror.rs` (the `pending` user entry, cleared by its `queued_command` or `user` record).

  Proof: tests against a recording PTY: a ready box gets the paste and Enter; a working session gets them too and the entry stays pending until its record; dialog, draft and overlay states answer `Error` and type nothing; with an agent in view the return recipe is typed first, and a reading that still shows the agent answers `Error` without the paste; the Enter waits for a reading that shows the paste in the box (CV.23 saw an Enter sent too soon left unsent). Depends on CV.7, CV.24.
- [ ] **CV.9 Answer prompts by typing their recipes.** `crates/daemon/src/server/chat.rs` (`ChatAnswer`), `crates/daemon/src/chat_mirror.rs` (prompt ids, the screen check after each key group, answered elsewhere), `crates/daemon/src/server/sessions.rs` (the owner-key clear shared with `SendInput`, D-A).

  Proof: tests per dialog fixture: the right bytes per answer; the deny-then-send ride-on waits for the box; a stale `prompt_id` is refused; a screen that does not move stops the recipe; no recipe sends Esc Esc; with an agent in view the return recipe runs before the answer's keys. Depends on CV.5, CV.8, CV.24.
- [x] **CV.10 Build the native chat model.** New `apps/native/src/chat.rs`, its `mod` line in `apps/native/src/lib.rs`: entry store with upsert and cap, card expansion, tool one-liners, answer builders, ghost stacking, the 400 ms digit grace. Proof: unit tests for upsert and cap, each answer kind's `ChatAnswerKind`, multi-select toggles, the grace window measured on an injected clock. Depends on CV.1.
- [x] **CV.11 Draw a session's chat in a pane.** New `apps/native/src/chat_view.rs`, `apps/native/src/grid_view.rs` (the pane body and the Terminal / Chat toggle in the pane header), `apps/native/src/lib.rs` (`Chat`, `ChatAppend`, `ChatScreenChanged` folds; `LoadChat` on show and reconnect, `UnloadChat` on the last pane), new `apps/native/tests/ui/ui_chat.rs` and its `mod` line in `apps/native/tests/ui/main.rs`.

  Proof: against the fake daemon, the toggle sends `LoadChat`; the reply renders; an append updates a tool card; a live answer grows; closing the last chat pane sends `UnloadChat`. Depends on CV.7, CV.10.
- [ ] **CV.12 Answer prompts and send messages from the chat.** New `apps/native/src/chat_cards.rs`, `apps/native/src/chat_view.rs`, `apps/native/src/chat.rs`, `apps/native/tests/ui/ui_chat.rs`: the cards, receipts, "Answered elsewhere", the ride-on choices, the pinned Heads up notice, the composer and its refusals.

  Proof: specs: each card sends the right `ChatAnswer`; digits wait out the grace; the composer during a dialog offers the two ride-on answers; a `Draft` state shows why it cannot send; a busy card stays until the dialog goes. Depends on CV.8, CV.9, CV.11.
- [x] **CV.13 Remember each session's view and reach it from the keyboard.** `apps/native/src/pane_menu.rs`, `apps/native/src/keys.rs`, `apps/native/src/sidebar.rs` (the last view per session in `native-ui.json`), `apps/native/src/grid_view.rs`, `apps/native/tests/ui/ui_chat.rs`. Proof: specs: the menu item and the key switch the view; a new pane on that session opens in the remembered view; a new session opens in Terminal; a pop-out keeps its view. Depends on CV.11.
- [ ] **CV.14 Prove the mirror end to end.** `crates/agents/claude/fake/index.mjs` (writes transcript lines, draws a permission dialog on a cue word, answers digits), `apps/native/tests/e2e_live.rs`, `apps/native/tests/support/live.rs` if the harness needs the transcript home. Proof: `.\rt.ps1 native-e2e` green with a spec that opens a chat, sends a message, allows the prompt from its card and sees the turn end, with the terminal pane open beside it. Depends on CV.12, CV.13.
- [ ] **CV.15 Document the mirror.** `CLAUDE.md` (the mirror under the architecture invariants, the chat messages' `request_id` echoes, `native-ui.json`'s last view), `docs/native-client.md`, `docs/architecture.md`, `docs/README.md` (glossary), `docs/plans/native-client-parity.md`, `docs/plans/borrowed-ideas.md` (its conversation-view item goes). Proof: the diff and a clean docs lint. Depends on CV.14.

### The running-work strip (RT-107)

- [ ] **CV.16 Keep the Stop payload's background tasks and wakeup.** `crates/agents/api/src/hook_vocabulary.rs` (the task and cron lists, beside the `background` flag), `crates/daemon/src/hook_status.rs`, `crates/daemon/src/session.rs` (the lists and first-seen stamps on the record), `crates/protocol/src/lib.rs` (`ChatWork` and its item types).

  Proof: mapping tests over the background-tasks spike's payloads: a shell with its command, a subagent with its type, a one-shot cron as a wakeup time, a recurring cron ignored; a task first seen keeps its stamp across later stops. Depends on CV.1.
- [ ] **CV.17 Count running work on the screen and send the strip.** `crates/agents/claude/src/dialogs.rs` (the running-agents, agents-panel and shell-count readers), `crates/agents/api/src/lib.rs`, `crates/daemon/src/chat_mirror.rs` (merging the record's lists with the screen's counts, ends on a later stop or a task notification), `crates/daemon/src/server/chat.rs` (`ChatWork` on load and on change), fixtures (`v71-agents-running`, the agents-panel and background-shell panes).

  Proof: fixture tests for the counts; server tests that `ChatWork` reaches loaders only and drops a finished task. Depends on CV.7, CV.16.
- [ ] **CV.18 Draw the strip over the composer.** New `apps/native/src/work_strip.rs`, `apps/native/src/chat_view.rs`, `apps/native/src/chat.rs`, `apps/native/src/lib.rs` (the `ChatWork` fold), `apps/native/tests/ui/ui_chat.rs`. Proof: specs: a scripted `ChatWork` shows its counts, a shell's age and the wakeup countdown; an empty one hides the strip; a screen-read count carries its tooltip; a subagent item is not tappable. Depends on CV.11, CV.17.

### The summary view (RT-111)

- [ ] **CV.19 Split the chat into stretches with template lines.** New `crates/daemon/src/summary_lines.rs`, `crates/protocol/src/lib.rs` (`SummaryLine` and the four summary messages, reusing RT-444's types where they fit), `crates/daemon/src/chat_mirror.rs` (entry-id ranges per stretch), `crates/daemon/src/server/chat.rs` and `crates/daemon/src/server.rs` (the handlers and arms), `crates/daemon/src/main.rs` (`mod` line).

  Proof: tests: one line per turn; a long turn splits into windows; each line's range expands to exactly its entries; the reply echoes `request_id`. Depends on CV.7 and RT-444.
- [ ] **CV.20 Write summary lines with the summarizer.** `crates/daemon/src/summary_lines.rs` and the summarizer's request API in `crates/daemon/src/summarizer/` (called, not changed): a line per stretch from the stretch's text, cached by text, under the spend cap, the template on a failure.

  Proof: tests against the summarizer's mock provider: a stretch gets its model line; an unchanged stretch makes no call; a failure or the cap keeps the template. Depends on CV.19 and the summarizer landing ([dashboard.md](./dashboard.md), RT-28).
- [ ] **CV.21 Draw the summary view.** New `apps/native/src/summary_view.rs`, `apps/native/src/grid_view.rs` (Summary as the third pane view), `apps/native/src/chat.rs` (expanding a line into its entries), `apps/native/src/lib.rs` (the summary folds), new `apps/native/tests/ui/ui_summary.rs` and its `mod` line in `apps/native/tests/ui/main.rs`.

  Proof: specs: lines render with the pinned question on top; a line expands into its entries; the strip shows; the view is remembered as the others are. Depends on CV.13, CV.18, CV.19.
- [ ] **CV.22 Document the strip and the summary view, and retire this file.** `CLAUDE.md`, `docs/native-client.md`, `docs/architecture.md`, `docs/README.md`, `docs/plans/off-orca.md` (SV.3 and SV.4); promote this file's lasting decisions into those docs and delete it. Proof: the diff and a clean docs lint. Depends on CV.15, CV.18, CV.21.
