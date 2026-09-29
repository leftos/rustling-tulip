# The "Needs You" view

Design for the "Needs You" item in [MAIN.md](./MAIN.md) (Wave 7). Origin and the user's placement ruling: [borrowed-ideas.md](./borrowed-ideas.md). Status source: [hook-status.md](./hook-status.md) (Wave 6). Summaries: [spoken-alerts.md](./spoken-alerts.md) (Wave 7, including the Dashboard it sits beside).

## Problem

With many sessions open, the only signs that one is waiting are the leaf's amber highlight and "!" in the Sessions panel (spread across containers, hidden inside folded ones) and a P4.3 toast that disappears. The Dashboard (DB.2) will answer "what is everything doing" in a full tab; nothing answers "who is waiting on me, and for how long" in one glance that stays beside the terminals.

## Rulings recap

- A view on the native client's activity rail, beside Sessions and Source control, not a sidebar filter (borrowed-ideas.md).
- Kept beside the Dashboard: the Dashboard groups needs-you sessions among all others, and this view stays as a compact always-visible list (spoken-alerts.md, Q5).
- Option buttons that answer a question belong to needs-you *cards*, driven by the hook's structured question data, with "Go to pane" as the fallback (spoken-alerts.md, Q6). The compact list carries none: a click goes to the pane (Q3).

## Design

### What counts as needing you

A session is listed when any of these holds, checked in this order (the first match names the row's reason). Parked (`is_inactive`) and abandoned (`is_abandoned`) sessions are never listed: Resume and the Recover dialog own them. Sessions excluded from busy tracking (P4.16) are listed like any other: the exclusion is about busy counts, and its ruling keeps their OS notifications (Q4).

| Reason | Condition | Source | Leaves the list when |
| - | - | - | - |
| **Asking** | `status == AwaitingInput` | today's heuristic, or hook status once Wave 6 lands | the status changes (answered, interrupted, ended) |
| **Answer** | `status == Idle`, `summary.kind == NeedsAnswer`, and `summary.updated_at >= status_since` (the summary is about this stop, not an older one) | Wave 7 summarizer (SA.6) | the status changes (a new prompt makes it `Working`) |
| **Ended** | `status` is `Error` or `Stopped`, and the session is in the client's attention set | today's `Attention { Error / Stopped }` | the user clicks the row or its leaf (the attention clears), or the session is discarded |

A row's membership comes from the snapshot, not from the attention set, for Asking and Answer: clicking an Asking row focuses the pane and clears the leaf's "!", but the row stays until the question is answered, since the session still needs you. Ended rows are the one kind a look dismisses.

`done_waiting` stops (a finished turn with nothing asked) and `working_update` are not listed; they are the Dashboard's Done and Working groups.

### A row

Two lines, the panel's width, no card chrome:

- Line 1: the status shape (the leaf's dot today; Petal's asking diamond once Wave 1 lands), the container name in its accent colour, " · ", the session's display label (the same label the leaf shows), and right-aligned the time waited ("12s", "4m", "1h 5m") since `status_since`.
- Line 2, muted, one line with an ellipsis, the full text in the tooltip: what it wants, from the best source present:
  - `pending_input` (Wave 6): Question → its `header` and first `question` ("Migration: keep newest, merge oldest, or skip?"), with "+2 more" when there are several; Permission → "Allow `<tool_name>`: `<summary>`"; PlanApproval → "Approve plan: " and the plan's first line; Other → its `message`.
  - `summary.headline` (Wave 7), for Answer rows and for Asking rows without `pending_input`.
  - Degraded (neither): Asking → "Waiting for input" plus the session's `terminal_title` when it has one (Claude titles the terminal with its task); Ended → "Error" or "Exited with code N" (`exit_code`).
- Hover: the row highlights; the tooltip carries line 2 in full.

### Order

Longest waiting first (`status_since` ascending); a session with no `status_since` (an older daemon) sorts after those that have one, then by sidebar order (the `containers()` walk). Ended rows come after every Asking and Answer row: a question blocks work, a finished session does not. The list is flat; no group headers.

### Clicking

A click is a leaf click: `RootView::select_session` (`lib.rs`), which focuses the pane showing the session (switching tab), or places the session when no pane shows it, and clears its attention. The panel stays on Needs You, and keyboard focus goes to the pane, so the user types the answer at once. Right-click opens the same session menu as the leaf (`session_menu.rs`).

### Rail item and badge

A third rail item after Sessions, before Source control, with its own icon (`assets/`, an inbox or hand glyph drawn to the rail's 18 px stroke). Its badge counts the listed rows through `badge_text` (hidden at 0, `99+`), in the leaf-attention amber (`WARNING`) rather than the accent, so it reads as "waiting" and not as "uncommitted". The rail is always shown, folded panel or not, so the badge is the always-visible part; the list is one click away. `item_tip` stops hard-coding "uncommitted": each item supplies its own noun ("3 waiting on you", "5 uncommitted"). The Recover dialog (High priority, in progress on a branch) also adds a rail button with a badge to `activity_bar.rs`, so NY.3 rebases onto it and extends its `Item` shape rather than adding a second one. There is no key binding for the view or for jumping to the longest-waiting session; the rail item and its badge are the entry point (Q5).

The rail does not switch to Needs You on its own when a session starts waiting (the badge and the toast already say so; taking the panel away from Sessions or Source control mid-task would be worse).

### Empty state

"Nothing needs you" in the panel's muted text, and under it "Sessions waiting on an answer, a permission or a look show here."

### Time waited

The label is minute-grained past a minute and second-grained below it. A 1 s repaint task runs only while Needs You is the visible panel and a row is under a minute old; otherwise 30 s. The task stops when the panel is folded or switched.

### What it needs from the daemon

One additive field, useful without Waves 6 and 7 and needed by the Dashboard's "Needs you sorted by longest waiting" (DB.2) as well:

- `SessionSnapshot.status_since: Option<DateTime<Utc>>`, `#[serde(default)]`: when `status` last changed. `SessionRegistry::update_from` (`crates/daemon/src/session.rs`) stamps it whenever the closure changes `status`, and record creation sets it. A reattached session after a daemon restart takes the restart time (not persisted: the sidecar keeps no status). Protocol 22 ignores the unknown field; `supported` does not change.

No new message. `pending_input` (HS.2) and `summary` (SA.1) arrive on the existing snapshot and `SessionUpdated` broadcast.

### Degraded mode and what each wave adds

- **Before Wave 6**: Asking rows from the `pty_state.rs` heuristic (a prompt regex match: permission prompts, numbered choices, `AskUserQuestion` framing); its idle check reclassifies the scrollback tail, so the state holds while the prompt is still on screen. Line 2 is "Waiting for input" plus the terminal title. Ended rows from today's attention events. A question asked in prose at the end of a turn is invisible (the session is just `Idle`).
- **With Wave 6**: Asking becomes reliable for hook-driven Claude sessions, and line 2 carries the real question, permission or plan. Codex, Cursor and hook-less sessions stay on the heuristic.
- **With Wave 7**: Answer rows appear (prose questions at a stop), and headlines fill line 2 where no `pending_input` exists.

## Steps

NY.1 to NY.3 need neither Wave 6 nor Wave 7 and land now, ahead of Wave 6, as the reduced view (Q1); NY.4 needs HS.2; NY.5 needs SA.1 (and shows Answer rows only once SA.6 fills summaries).

- [x] **NY.1 `status_since` on the snapshot.** `crates/protocol/src/lib.rs`: the field. `crates/daemon/src/session.rs`: stamp in `update_from` when `status` differs from before the closure, and at record creation; audit the direct `rec.status =` writes outside `update_from` (`rg -n "\.status = " crates/daemon/src`) and route any that bypass it. Proof: a protocol round-trip test with and without the field, `cargo test -p protocol v22_compat`; a daemon unit test that a status change stamps a new time, a same-status update keeps the old one, and a new record has one. Gates: `-p protocol -p daemon`.
- [x] **NY.2 The list model.** New `apps/native/src/needs_you.rs` (plain Rust, no GPUI): `NeedsYouRow { session_id, container_name, accent, label, reason: Reason, since, detail: String }`, `enum Reason { Asking, Answer, Ended }`, `fn rows(model: &SidebarModel) -> Vec<NeedsYouRow>` built from `containers()` (sidebar order, display labels, the attention flag) and the snapshots; the membership table, the order, the degraded line-2 text, and `fn waited(now, since) -> String`. Proof: unit tests for each membership row (including parked and abandoned excluded, and an Asking row that survives a cleared attention), the order (oldest first, no-`since` after, Ended last, sidebar order on ties), line 2 with and without a terminal title and for each Ended form, and `waited` at 0 s, 59 s, 60 s, 1 h 5 m. Gates: `-p rustling-tulip-native`. Needs NY.1.
- [ ] **NY.3 Rail item and panel.** `apps/native/src/sidebar.rs`: `Activity::NeedsYou`, with `Sessions` made the `#[default] #[serde(other)]` last variant as `SidebarView` does, so a future unknown value loads as Sessions. `activity_bar.rs`: the third item, its icon in `assets.rs`, the amber badge, `item_tip` with a per-item noun. `sidebar_view.rs`: the `main_row` match arm. New `apps/native/src/needs_you_view.rs`: rows, empty state, click through `select_session`, right-click menu, the repaint task. `tests/support/mod.rs`: `SessionBuilder::status_since`. Proof: a new `tests/ui_needs_you.rs` against the scripted fake daemon: three sessions (two `awaiting_input` with different `status_since`, one working) list the two oldest-first; the rail badge reads "2" while the panel is folded and while Sessions shows; a click focuses the session's pane and the row stays; an `Attention { Error }` adds an Ended row that a click removes; a `SessionUpdated` to `working` drops a row; no rows shows the empty text; the activity persists in `native-ui.json` and an unknown value loads as Sessions. Existing `ui_sidebar` and `ui_source_control` specs stay green. Gates: `-p rustling-tulip-native`; hand-test the icon, the badge colour and the row layout at the narrowest sidebar width. Needs NY.2.
- [ ] **NY.4 Hook detail** *(needs HS.2)*. `needs_you.rs`: line 2 from `pending_input` per variant, "+N more" for several questions. `tests/support/mod.rs`: `SessionBuilder::pending_input`. Proof: unit tests per `PendingInput` variant including `Unknown` falling back to the degraded text; one `ui_needs_you.rs` case showing a Question's header and first question.
- [ ] **NY.5 Summaries** *(needs SA.1)*. `needs_you.rs`: the Answer reason with the `updated_at >= status_since` check; `summary.headline` as line 2 where `pending_input` is absent. Proof: unit tests: an `Idle` session with a fresh `NeedsAnswer` summary is listed, one whose summary predates `status_since` is not, `DoneWaiting` is not, a `Working` session with a `NeedsAnswer` summary is not; one `ui_needs_you.rs` case with a headline.
- [ ] **NY.6 Docs.** `docs/native-client.md` (a Needs You section: membership, order, badge), `docs/architecture.md` task index (the view's files), the README glossary ("Needs You", "status_since"), CLAUDE.md's `native-ui.json` line if the `activity` values are listed; delete the MAIN.md line and borrowed-ideas.md's entry, and this doc, once NY.5 lands. Proof: the diff.

## Open questions

Answered (user): Q1 (a) NY.1–NY.3 land now, ahead of Wave 6, as the reduced view, and NY.4 and NY.5 follow their waves; Q2 (a) `Error` and attention-flagged `Stopped` rows are listed after the waiting ones and dismissed by a click; Q3 (a) no answer buttons in the compact rows; Q4 (a) sessions excluded from busy tracking are listed like any other; Q5 (a) no shortcut to the longest-waiting session.

1. **When NY.1–NY.3 land.** (a) Recommended: as soon as they are free, ahead of Wave 6, as the degraded view; NY.4 and NY.5 follow their waves. How: the MAIN line splits into a now-line (NY.1–NY.3) and a Wave 7 line (NY.4–NY.5). Worst case: the heuristic misses a prompt whose wording changed, and the user learns to trust a list that is quietly incomplete until Wave 6. (b) Keep all of it in Wave 7. Worst case: the rail item waits behind the summarizer and key-store work for a list that needs neither.
2. **Ended rows.** (a) Recommended: list `Error` and attention-flagged `Stopped` sessions after the waiting ones, dismissed by a click. How: the attention set already tracks them. Worst case: a batch of sessions finishing together pads the list and the badge until each is clicked. (b) Waiting only (Asking and Answer). Worst case: a session that crashed is only in the sidebar's "!" and a toast, the thing this view exists to replace. (c) Error only. Worst case: a clean exit the user was waiting for goes unlisted.
3. **Option buttons in the compact row** (spoken-alerts Q6 ruled them for needs-you cards). (a) Recommended: none in the compact list; a click goes to the pane, and the Dashboard's cards carry the buttons (no DB step names them yet, so DB.2 or a new DB line owns them). Worst case: one click and a keystroke instead of one click. (b) The same buttons under line 2 when `pending_input` is a Question or Permission whose options map safely to keys, else none. Worst case: a narrow panel wraps the buttons badly, and two answer surfaces must track any change to the `AskUserQuestion` TUI.
4. **Sessions excluded from busy tracking (P4.16).** (a) Recommended: listed like any other; the exclusion is about busy counts, and its ruling keeps their OS notifications. Worst case: a dev server whose output matches the heuristic's prompt regex sits in the list. (b) Never listed, as they leave the leaf highlight. Worst case: an excluded Claude session that really asks is missed.
5. **A shortcut to the longest-waiting session.** (a) Recommended: none now; the rail item and its badge are the entry point. Worst case: reaching a waiting session always takes the mouse, since the list has no keyboard navigation. (b) A "Go to next waiting session" action with a key binding (for example Ctrl+Shift+J), cycling in list order. Worst case: one more binding to keep clear of terminal apps' keys.
