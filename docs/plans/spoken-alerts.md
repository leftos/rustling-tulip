# Spoken alerts, session summaries and the Dashboard

Design for the "Spoken and phone alerts when an agent waits" item in [plan.md](../plan.md), widened to a shared summarizer service that also feeds a Dashboard view. Evidence: [spikes/waiting-alerts.md](../spikes/waiting-alerts.md). Status source: [hook-status.md](./hook-status.md) (hook-reported agent status), which this design consumes and does not redesign.

## Problem

The user runs many Claude sessions at once and is often not looking at the screen. Today the only signal that one is waiting is P4.3's OS toast ("Claude is awaiting input" plus the session label, `apps/native/src/notify.rs`), fired from the `pty_state.rs` heuristic. It says nothing about what the session wants, can't be heard from across the room, and finding out means reading the terminal. The same gap exists when the user does look: there is no view that says what every session is doing without opening each terminal.

## Rulings recap

- Summarizer default: DeepSeek Flash (`deepseek-flash` via `https://api.deepseek.com/anthropic`) at its default effort. Settings offers at least DeepSeek and Anthropic, each keyed by a masked API-key field or the name of an environment variable holding the key.
- The daemon calls the provider's Messages-compatible API directly: an exception to CLAUDE.md's no-direct-API rule, changed in the landing commit.
- One call returns the summary and classifies the stop as `needs_answer`, `working_update` or `done_waiting`; each kind has its own notification settings.
- Every alert is prefixed with the repo's or workspace's name, or a spoken name the user sets per container.
- Speech: WinRT `SpeechSynthesizer`, offline, voice picked in Settings. Phone alerts wait for the mobile app's MA7 push; no interim service.
- Triggers: a turn that finishes awaiting input, a permission prompt, an `AskUserQuestion` interview.
- The summarizer also serves a Dashboard view (a summary of every active session, including needs-you state), offered in the native client and the mobile app.

## Design

### Inputs from hook-status

[hook-status.md](./hook-status.md) delivers per-session hook events to the daemon. This design uses three, with the fields the Claude Code hooks reference documents (`code.claude.com/docs/en/hooks`):

| Event | Field used | Trigger |
|---|---|---|
| `Stop` | `last_assistant_message` (the final text of the turn; the docs say to prefer it over the transcript, which may lag at Stop time) | `stop` |
| `Notification` with `notification_type: "permission_prompt"` | `message` (for example "Claude needs your permission to use Bash") | `permission` |
| `PreToolUse` with `tool_name: "AskUserQuestion"` | `tool_input.questions[]`: `question`, `header`, `options[].label`, `multiSelect` | `question` |

Claude Code only fires `permission_prompt` after the prompt has waited about six seconds with no keystroke, so a user who is answering is never alerted. The daemon applies the same gate to `stop` and `question`: the alert is held six seconds from the event and dropped if the session receives PTY input first. Only interactive Claude sessions alert; headless, plain-shell and Codex sessions do not (no hook events).

### The summarizer service (daemon)

A new module `crates/daemon/src/summarizer/` owns every model call. It has two consumers: the alert pipeline and the per-session summary state the Dashboard reads. Both go through one queue, one spend ledger and one cache.

- **Request**: `POST {base}/v1/messages` with `x-api-key`, `anthropic-version: 2023-06-01`, `max_tokens: 1500` (DeepSeek replies carry a thinking block of 200 to 700 tokens in the spike), one user message. HTTP through `reqwest` 0.12 with `rustls-tls`, already in `Cargo.lock` (0.12.28) via `crates/daemon-client`, so the tree gains no crate.
- **Providers** (a table in code, not user-editable): DeepSeek, `deepseek-flash`, default env var `DEEPSEEK_API_KEY`, $0.30 / $1.20 per million tokens; Anthropic, `claude-haiku-4-5`, default env var `ANTHROPIC_API_KEY`, $1 / $5. Prices turn each reply's `usage` into dollars for the ledger.
- **Output**: JSON `{"kind": "needs_answer|working_update|done_waiting", "headline": "...", "speech": "...", "notification": "..."}`. `headline` is at most 8 words for a Dashboard card; `speech` at most 25 words for a voice; `notification` at most 100 characters. Anthropic gets the schema through structured outputs (`output_config.format`, the spike's own next step); for DeepSeek, whose Anthropic-compatible endpoint is not verified to accept it, the parser strips a code fence and reads the first JSON object. Any field over its cap is cut at a word boundary; an unknown `kind` makes the reply a failure.
- **Prompt**: v3 of the spike's `prompt-v2.md`, moved to `crates/daemon/src/summarizer/prompt.md` and embedded with `include_str!`. Changes from v2: the `Session:` line goes (no container or session name leaves the machine), a rule to spell out or drop plan numbers and function names (the spike's leak), the `headline` field, and a `Trigger:` line. For `question`, the daemon overrides `kind` to `needs_answer`; for the Dashboard's `working` refresh (below) the prompt asks what is in progress.
- **What is sent**: for `stop`, `last_assistant_message`; for `question`, the questions rendered as plain lines (question, then its option labels; descriptions dropped); for a `working` refresh, the last assistant text in the transcript tail. Capped at 6,000 characters: the first 1,000 and the last 5,000 are kept with a `[…]` between, as questions sit at the end. Strings shaped like secrets (`sk-…`, `ghp_…`, `github_pat_…`, `xox[bp]-…`, `AKIA…`, PEM blocks) are replaced with `[redacted]` before sending. `permission` sends nothing: its text is Claude Code's own `message`.
- **Privacy**: the text goes to the chosen provider under its terms; for DeepSeek that is a provider in China. The Alerts tab says so in one line under the provider choice. Nothing is sent until the user saves a key or names a variable, so the feature is opt-in by construction.
- **Timeouts and retries**: 3 s connect, 8 s total (the spike measured 1.0 to 3.2 s). One retry after 1 s on a timeout, a connection error, 429 or 5xx; none on another 4xx. A 401 or 403 marks the provider's key `rejected` in the status sent to Settings, and no further call is made until the key or variable changes.
- **Concurrency and cache**: at most four calls in flight; a new event for a session cancels that session's queued (not in-flight) request. Results are cached by (trigger, SHA-256 of the sent text), 64 entries, so an unchanged `working` refresh costs nothing.
- **Spend cap**: a daily cap in dollars (default $0.50, about a thousand DeepSeek alerts at the spike's measured cost), counted in local days in `alert-spend.json` in the config dir. Dashboard `working` refreshes stop at half the cap; alerts stop at the cap. Past it, alerts fall back to templates and one notice tells the connected clients.
- **No key, a failure, or the cap**: templates, no model call. `stop`: "is waiting for you" with no kind (treated as `needs_answer` by every setting, erring towards alerting); `permission`: Claude Code's `message`; `question`: "has a question:" plus the first question's `header`. The alert's `source` says `template` so Settings and logs can tell.
- **Idle**: no call is made while no client is connected and no phone channel exists (the git watcher parks on the same `Hub.client_count`).

### Keys

A typed key is stored in **Windows Credential Manager** through `keyring-core` 1.0.0 with `windows-native-keyring-store` 1.1.0 (default features off, which drops its `search` feature; it depends on `windows-sys` 0.61, `zeroize` and `byteorder`), service `rustling-tulip`, user `summarizer/<provider>`. The `keyring` 4.2.0 umbrella is not taken: it pulls every platform's store. On macOS (see [macos-compat.md](./macos-compat.md)) the same code takes `apple-native-keyring-store` 1.0.2. Reasons over a DPAPI-encrypted file (`CryptProtectData` behind the `Win32_Security_Cryptography` feature of the `windows` 0.61 crate the daemon already uses): the user can see and delete the key in Credential Manager, there is no file format of ours to get right, and the macOS path is the same API. (`windows-dpapi` 0.2.0 was ruled out: it encrypts at machine scope, readable by any user on the machine.) Tests use `keyring_core::mock`.

- The key is set with a write-only `SetProviderKey` and never leaves the daemon again: status carries only `missing`, `stored`, `env_found`, `env_missing` or `rejected`. Its type redacts itself in `Debug` and is never logged.
- An environment variable is read at call time from the daemon's environment, falling back to `HKCU\Environment` (the daemon started at login keeps the environment it was born with; the spike's `api.py` does the same).
- A paired LAN client may set a key: pairing already grants spawning a shell, so this adds no power.
- Never in `state.json` or `native-ui.json`. `state.json` gains a secret-free `summarizer` block: `enabled`, `provider`, per-provider key source (`stored` or `env` with a variable name), `daily_cap_usd`, `dashboard_refresh`.

### Protocol (additive; protocol 22 stays decodable)

New types in `crates/protocol/src/lib.rs`, each nested enum with `#[serde(other)] Unknown`:

- `AlertKind { NeedsAnswer, WorkingUpdate, DoneWaiting }`, `AlertTrigger { Stop, Permission, Question }`, `SummarySource { Model, Template }`.
- `SessionSummary { kind: Option<AlertKind>, headline, detail, pending: Option<PendingAsk>, updated_at, source }`, where `PendingAsk { trigger, text, options: Vec<String> }` is the verbatim question or permission text (not summarized).
- `SessionSnapshot.summary: Option<SessionSummary>` (`#[serde(default)]`): summary changes ride the existing `SessionUpdated` broadcast, keeping `SessionSnapshot` the one session shape. Persisted as `sessions/<id>/summary.json` so a daemon restart doesn't blank the Dashboard or re-pay calls.
- `DaemonMessage::WaitingAlert { alert_id, session_id, trigger, kind: Option<AlertKind>, prefix, speech, notification, source, request_id }`: the one-shot event clients speak. `prefix` is resolved on the daemon (spoken name, else container name) so every client and MA7's push say the same thing.
- `DaemonMessage::Attention` gains `alert_id: Option<String>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`): an alert for this attention follows.
- `ClientMessage::SetSummarizerSettings { settings }`, `ClientMessage::SetProviderKey { provider, key: Option<String> }` (`None` deletes), `ClientMessage::TestAlert { request_id }` (the daemon summarizes a fixed sample message and answers the requester alone with a `WaitingAlert` or an `Error`), `ClientMessage::WatchDashboard { on }` (turns `working` refreshes on for that connection), `DaemonMessage::SummarizerStatus { settings, keys, spend_today_usd, cap_reached }`, broadcast on change and on connect.
- `RepoEntry.spoken_name` and `WorkspaceEntry.spoken_name`: `Option<String>`, `#[serde(default)]`, set by `SetRepoSpokenName` / `SetWorkspaceSpokenName` like the existing `SetRepoAppearance` pair.

`cargo test -p protocol v22_compat` proves each change leaves v22 decodable.

### Speech, toasts and debouncing (native client)

Speech runs in the **native client**: it has the audio device and knows whether the user is at this window; the daemon has the events and sends `WaitingAlert`. A new `apps/native/src/speech.rs` mirrors `notify.rs`: a `Speaker` trait, a WinRT implementation (`windows` features `Media_SpeechSynthesis`, `Media_Core`, `Media_Playback`, `Storage_Streams`: `SpeechSynthesizer::SynthesizeTextToStreamAsync`, played through a `MediaPlayer` on a background thread; `SpeechSynthesizer::AllVoices` lists voices), and a `SilentSpeaker` for the cloaked smoke window and tests.

- **Settings per kind** in `native-ui.json` `alerts`: `speak` and `toast` for each of the three kinds (defaults: speak on for `needs_answer` and `done_waiting`, off for `working_update`; toast on for all), and `voice` (a voice id; `None` is the system default). Phone per kind is added with MA7, daemon-side.
- **Queue**: alerts wait 1.5 s after the first arrives, then speak in order. A newer alert for the same session replaces its unspoken one. Three or more waiting at speak time become one line: "Three sessions are waiting: tulip, acorn and docs." An alert is dropped unspoken if its session has run again or been removed since.
- **Presence**: nothing is spoken for a session whose pane is focused in the foreground window with a keystroke in the last 30 s.
- **Toasts (P4.3)**: an `Attention` carrying `alert_id` holds its toast until the matching `WaitingAlert` (up to 10 s), then shows it titled with the prefix and the kind ("tulip needs your answer") and the alert's `notification` as body, filtered by `awaiting_input` and the kind's `toast` toggle. Without `alert_id`, or on the timeout, the toast is as today.
- **P4.16 (Don't count as busy)**: no effect. Its ruling keeps OS notifications for excluded sessions, and alerts follow the same line; an always-busy session is usually a plain shell with no hook events anyway.

### Settings UI

A new **Alerts** tab after Notifications in `settings_view.rs` (the Notifications tab keeps its three toggles and the Windows link):

1. Summarizer on / off; provider segmented (DeepSeek, Anthropic) with its privacy line; key source segmented (Stored key, Environment variable); a masked `text_input` with Save and Clear, or a variable-name field; the key state (`stored`, `found`, `not set`, `rejected`).
2. Daily cap (dollars) and today's spend.
3. Voice picker (`combobox.rs`, listing `AllVoices` by display name and language).
4. A kinds grid: rows `needs_answer`, `working_update`, `done_waiting` in plain words, columns Speak and Toast.
5. Test: sends `TestAlert`, speaks the reply with the chosen voice, shows its source, latency and cost.

Spoken names are edited in the container's Appearance dialog (`appearance_view.rs`) as a "Spoken name" field under the name, placeholder the container's name.

## Dashboard view

A view of every live session as a card, fed by `SessionSnapshot.summary` and the fields the daemon already sends, so the user learns what is going on without reading terminals.

- **Placement**: a Dashboard tab kind in the tab area (like diff tabs, `diff_tab.rs`), opened from an activity-rail icon; its tab opening sends `WatchDashboard { on: true }`, closing sends `false`.
- **Card**: the accent-coloured container name and session label; status with needs-you first (from `summary.kind` and the hook-reported status); `headline`, with `detail` on expand; branch and worktree; runtime (since `started_at`) and time since the last activity (`metrics.last_activity_at`); tokens and cost (`metrics`); the `pending` question or permission text in full with its option labels and a "Go to pane" action that focuses the session's pane (opening a tab for it when it has none).
- **Grouping**: Needs you, Working, Done, Idle (the "Needs You" item's grouping); Needs you sorted by longest waiting, the rest by latest activity. A toggle groups by container instead, in sidebar order.
- **Header strip**: the day's session cost (summed `metrics.cost_usd`) and summarizer spend, and counts per group.
- **Files changed**: each card shows the worktree's changed-file count and added / removed lines from the daemon's existing git status read of the session's worktree.
- **Activity timeline**: an optional lane per session over the last few hours, coloured by status (running, waiting, idle), from status transitions the daemon keeps in memory (bounded, not persisted).
- **Refresh**: summaries refresh on every `stop` (the alert's own call), and while a Dashboard is watched, every 3 minutes for a running session whose transcript grew since its last summary, capped by the half-cap rule and cached by text hash.
- **Mobile (MA6)**: MA6's session list shows the same cards compactly (headline, status, pending text) from the same snapshot field, with no mobile-side model call; "Go to pane" becomes MA6's own session screen, where replying and answering permission prompts already live. MA7's push reuses `WaitingAlert`'s `prefix` and `notification`.

## Open questions

Answered (user): Q3 every 3 min while watched and the transcript grew, plus a summary on every stop; Q4 the Dashboard is a tab (a rail button or shortcut opens or focuses it); Q5 keep both: the Dashboard groups needs-you sessions, and the separate "Needs You" item stays as a compact always-visible list; Q1 client only, silent while the native client is closed; Q6 option buttons: a needs-you card shows the question's options as buttons that answer it (the design must drive them from the hook's structured question data, and fall back to "Go to pane" when the options can't be mapped to keys safely); Q7 skip speech for the focused pane after a keystroke in the last 30 s; Q8 a $0.50 daily cap, editable in Settings. Settled (orchestrator, technical): Q2 Credential Manager via `keyring-core`.

1. **Speech with no window open.** (a) Client only (recommended); worst case: alerts are silent while the native client is closed. (b) The daemon speaks when no client is connected; worst case: a client connecting mid-sentence speaks the same alert again.
2. **Key store.** (a) Credential Manager via `keyring-core` (recommended); worst case: three small new crates to vet in `deny.toml`. (b) DPAPI file in the config dir; worst case: a hand-rolled file format and a separate macOS answer.
3. **Periodic Dashboard refresh.** (a) Every 3 min while watched and the transcript grew (recommended); worst case: about 20 calls an hour per busy session, roughly a cent. (b) Stop events only; worst case: a long-running session's card stays stale for its whole turn.
4. **Dashboard placement.** (a) A tab (recommended); worst case: one more tab to manage. (b) An activity-rail panel beside the terminals; worst case: cards squeezed into a narrow column.
5. **The "Needs You" plan item.** (a) Fold it into the Dashboard's grouping (recommended); worst case: someone wanting a tiny always-visible list gets a full view. (b) Keep both; worst case: two views of the same state to maintain.
6. **Answering from a card.** (a) Leave answering to "Go to pane" and the planned conversation view (recommended); worst case: one extra click. (b) Option buttons that type into the PTY; worst case: keystrokes that no longer match a changed `AskUserQuestion` TUI answer the wrong option.
7. **Presence suppression.** (a) Skip speech for the focused pane with a keystroke in the last 30 s (recommended); worst case: a user reading but not typing hears the alert anyway. (b) Always speak; worst case: speech about the session the user is typing in.
8. **Default daily cap.** (a) $0.50 (recommended); worst case: a heavy day hits templates late in the evening. (b) No cap; worst case: a runaway loop of stops spends without bound.

## Checklist

Steps marked *(hook-status)* need [hook-status.md](./hook-status.md)'s events first. The CLAUDE.md exception lands with SA.6.

- [ ] **SA.1 Protocol types.** `crates/protocol/src/lib.rs`: the types, fields and messages above. Proof: round-trip tests for each, an `Unknown` test per new nested enum, and `cargo test -p protocol v22_compat` still passing.
- [ ] **SA.2 Key store.** `crates/daemon/src/summarizer/keys.rs`, `crates/daemon/Cargo.toml`, `deny.toml` if a licence needs listing: `KeySource` resolution (env, then `HKCU\Environment`), Credential Manager set / get / delete, redacting `Debug`. Proof: unit tests on `keyring_core::mock`, including delete and a missing variable, and a test that `format!("{:?}")` of a key shows no key text.
- [ ] **SA.3 Provider client.** `crates/daemon/src/summarizer/client.rs`: request, fence-tolerant parse, caps, timeouts, the retry rule, `rejected`, cost from `usage`. Proof: tests against a local axum mock covering 200, fenced JSON, 429 then 200, 401, a timeout, and JSON with an unknown `kind`.
- [ ] **SA.4 Prompt v3 and input shaping.** `crates/daemon/src/summarizer/prompt.md`, `prompt.rs`: head / tail cap, redaction, question rendering, trigger line. Proof: unit tests for each; then rerun the spike's `api.py` with v3 on its three messages and add the results to the spike doc.
- [ ] **SA.5 Settings, ledger and cache.** `crates/daemon/src/state.rs`, `summarizer/mod.rs`, `server.rs`: the `summarizer` block, `alert-spend.json`, the cache, the four-call limit, `SetSummarizerSettings`, `SetProviderKey`, `TestAlert`, `SummarizerStatus`. Proof: server tests that a key set is never echoed, the cap switches to templates, the half-cap stops refreshes, and `TestAlert` answers only its requester.
- [ ] **SA.6 Alert pipeline** *(hook-status)*. `crates/daemon/src/summarizer/alerts.rs`, `session.rs`: the three triggers, the six-second hold dropped on input, templates, `Attention.alert_id`, `WaitingAlert`, `SessionSnapshot.summary` and `summary.json`, the idle rule; CLAUDE.md's no-direct-API line and on-disk list. Proof: a daemon test feeding fake hook events against the mock provider and asserting the WebSocket client sees `Attention` then `WaitingAlert`, and none when input arrives inside the hold.
- [ ] **SA.7 Spoken names.** `crates/daemon/src/registry.rs`, `server.rs`, `apps/native/src/appearance_view.rs`: the two messages and the dialog field. Proof: a registry test that the name persists and reaches `prefix`; a `ui_*` spec that editing the field sends `SetRepoSpokenName`.
- [ ] **SA.8 Speaker.** `apps/native/src/speech.rs`, `apps/native/Cargo.toml`: trait, WinRT speaker, voice list, `SilentSpeaker` wired where `SilentNotifier` is. Proof: an `#[ignore]`d test that lists at least one voice and synthesizes a short line to a stream; the smoke tier stays silent.
- [ ] **SA.9 Alert queue and toast merge.** `apps/native/src/alerts.rs`, `notify.rs`, `sidebar.rs` (`native-ui.json` `alerts`): coalescing, replacement, drop on rerun, presence rule, per-kind toggles, the held toast. Proof: a new `tests/ui_alerts.rs` with a recording `Speaker` and `Notifier` against the scripted fake daemon covering each rule and the 10 s toast fallback.
- [ ] **SA.10 Alerts tab.** `settings_view.rs`, `text_input.rs` (a masked mode), `combobox.rs`: the tab as described. Proof: a `ui_*` spec that Save sends `SetProviderKey` and clears the field, the key state follows `SummarizerStatus`, and Test speaks the reply through the recording `Speaker`.
- [ ] **SA.11 Docs.** `CLAUDE.md` (the on-disk list gains `alert-spend.json` and `summary.json`; `native-ui.json` gains `alerts`), `README.md` glossary (alert kind, summarizer, Dashboard), plan index line.
- [ ] **DB.1 Working refresh** *(hook-status)*. `crates/daemon/src/summarizer/refresh.rs`, `transcripts.rs`: `WatchDashboard`, the 3-minute tick for watched running sessions, transcript-tail input, cache hits. Proof: a paused-clock test that an unchanged transcript makes no call and a grown one makes one, and none with no watcher.
- [ ] **DB.2 Dashboard tab and cards.** `apps/native/src/dashboard.rs`, `dashboard_view.rs`, `tabs.rs`, `activity_bar.rs`: the tab kind, cards, grouping and sort, the header strip, "Go to pane". Proof: a `tests/ui_dashboard.rs` spec that snapshots group membership and order from scripted sessions and that "Go to pane" focuses the pane.
- [ ] **DB.3 Files changed per card.** `dashboard_view.rs` plus the daemon's git status read for worktree paths. Proof: a spec that a scripted status shows its counts on the card.
- [ ] **DB.4 Activity timeline.** `crates/daemon/src/session.rs` (bounded transition log, an additive field or message), `dashboard_view.rs`. Proof: a daemon test on the log's bound and a spec on lane colours.
- [ ] **DB.5 Mobile cards.** Mobile app's MA6 list: render `summary` in rows. Tracked in [mobile-app.md](./mobile-app.md) once MA6 starts.
