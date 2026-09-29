# Resume sessions after a reboot

Design for the "Resume the sessions a reboot killed, on the next start" line in [MAIN.md](./MAIN.md). Origin: [borrowed-ideas.md](./borrowed-ideas.md), "Resume agents after a reboot". Builds on session recovery (the history in `crates/daemon/src/history.rs`, `RecoverSessions`, the native Recover dialog in `apps/native/src/recover.rs` / `recover_view.rs`). Step ids are `RB.n`.

## Problem

A reboot kills every `rt-tracer.exe` and the daemon with them. On the next start the daemon finds their sidecars dead and lists them as abandoned; nothing asks the user whether to bring them back, and the user has to find the Recover dialog or each leaf's Resume. Worse, a session whose daemon died before it saw the tracer go gets no history entry at all, so it is missing from the Recover dialog and its badge.

## Rulings

- Ruling (user, 2026-09-28): prompt on start ("Resume all", "Choose…", "Dismiss"); a dismissed prompt leaves the sessions in Recover. Never resume without asking.
- How to tell a reboot from a user Stop: the sessions whose tracers were lost with no end recorded.

## Design

### What the code does today (measured)

- Every end path that deletes a sidecar records an end first: a Stop or Discard writes `StoppedByUser` (`server.rs` `record_end`), the quit dialog's Stop buttons drain and write `DaemonShutdown` (`shutdown_all`), a child exit writes `Exited` (`session.rs` `watch_exit`). "Abandon & quit" and HTTP `/shutdown` do not drain: the tracers keep running and the next daemon reattaches them.
- A tracer lost while the daemon runs: `watch_exit` writes `TracerLost`, keeps the sidecar and marks the session abandoned.
- At startup `main.rs` splits the sidecars with `orphan::partition_live`; the dead ones go to `SessionRegistry::insert_abandoned` (`server.rs` `reattach_orphans`), which writes no history entry. `history::import_tracer_logs` skips every live or dead sidecar id. So a session whose daemon died no later than its tracer (a reboot, `rt.ps1 stop`) sits in the Abandoned group with no history entry: it is not in the Recover dialog and not in its badge. In a reboot the order in which Windows kills the daemon and the tracers decides which sessions get a `TracerLost` entry and which get none.
- After a reboot the daemon starts at login (`--detach`) with no client; `idle_exit` can stop it 30 s later, and the next client start runs a fresh daemon over the same dead sidecars. Anything the first start works out in memory is lost by then, and its history writes mean "no end recorded" is no longer true on the second start.
- The previous daemon's log survives as `logs/daemon.log.old`; `tracer_log::parse_tracer_log` gives each tracer log's `last_line_at`.

### Which sessions are offered

A sidecar still on disk and dead at startup is, by the code above, a session that no user Stop, drained quit or child exit ended. At each start, before `import_tracer_logs`, the daemon:

1. Writes a `TracerLost` history entry for each dead sidecar with no entry (`history::entry_from_meta`: the sidecar's label, kind, members, spawn config, conversation id; `ended_at` from the tracer log's `last_line_at`, else the sidecar's modified time; `end_time_known: false`). Headless sidecars are skipped, as the history skips them. This alone puts these sessions in the Recover dialog and its badge.
2. Picks the offer set by Open question 1's rule (recommended: the sidecars step 1 wrote, plus dead sidecars whose unrecovered `TracerLost` entry ended within 60 s of the last line of `daemon.log.old`).
3. Adds the set to the pending offer in `state.json` (`PersistedState.resume_offer: Option<StoredOffer { id, session_ids }>`, `#[serde(default)]`), or starts a new one with a fresh id when none is pending. The offer lives in `state.json` so it outlasts an idle exit and a daemon restart.

An offer row drops when its entry is recovered (from the prompt, the dialog or the sidebar's Resume) or pruned after 7 days; the offer clears when no row is left or a client answers it.

### Protocol (additive, no bump)

- `ResumeOffer { id: String, session_ids: Vec<String> }`.
- `DaemonMessage::ResumeOffer { offer: Option<ResumeOffer> }`: sent in `push_initial_state` after `Sessions`, and broadcast to every client (a new `StateEvent`) when it changes; `None` means no prompt.
- `ClientMessage::AnswerResumeOffer { offer_id: String }`: clears the pending offer when the id matches, persists, broadcasts `None`; a stale id is ignored. All three buttons send it; "Resume all" also sends `RecoverSessions`.
- Protocol 22 (the installed Tauri app) receives the new message type as `Unknown` and never sends the new request. `supported` does not change; `cargo test -p protocol v22_compat` stays green.

### Native client (testable core, thin view)

- **Model** `apps/native/src/resume_offer.rs` (plain Rust): holds the last offer and reads `recover::SessionHistory`. `prompt()` returns `None` until both the offer and the history have arrived, or while another modal (the layout chooser, the quit dialog, the Recover dialog) is open; else `Prompt { title, rows: Vec<(label, place)>, resume_all: Vec<RecoverItem>, left_out: usize }`. `resume_all` uses the Recover dialog's own defaults for each offered row (its known conversation else the newest, its default "Recover as"; a plain shell reopens as a shell in its folder); a row the dialog would disable (no conversation found) is left out and counted. `recover.rs` exposes the defaults as `default_items(items, names, now, ids)` and gains `RecoverDialog::for_offer(…, ids)`, which ticks only the offered rows.
- **Actions**: Resume all → `RecoverSessions` with `resume_all`, then `AnswerResumeOffer`; the answer goes through the Recover dialog's existing outcome path (sessions placed by `spawns.rs` `place_several`, abandoned panes rebound by the daemon), and a failure opens the dialog on the failed rows. Choose… → `AnswerResumeOffer` and the Recover dialog opened with `for_offer`. Dismiss (and Esc) → `AnswerResumeOffer`; the rows stay in Recover and its badge.
- **View** `apps/native/src/resume_offer_view.rs`: renders `Prompt` as Open question 3 settles (recommended a modal); copy: title "Resume N sessions?", body "These were running when rustling-tulip last stopped (a restart or a crash).", up to 8 rows then "+N more", and "M can't be resumed automatically; they stay in Recover." when `left_out > 0`. Mounted in `lib.rs` (`RootView`); `net.rs` routes the message.

## Steps

Order: RB.1 → RB.2; RB.3 in parallel with both; RB.4 needs RB.2 and RB.3; RB.5 needs RB.3; RB.6 needs RB.4 and RB.5; RB.7 needs RB.6; RB.8 last.

- [ ] **RB.1 History entries for sessions lost with no end recorded.** `history::entry_from_meta` and `history::record_lost_at_start(dirs, dead) -> Vec<String>` (the ids it wrote); `main.rs` calls it before `import_tracer_logs`. Files: `crates/daemon/src/history.rs`, `main.rs`. Proof, red first: `history::tests::lost_at_start_writes_tracer_lost_without_entry`, `lost_at_start_keeps_existing_entry`, `lost_at_start_skips_headless`, `lost_at_start_end_time_from_tracer_log_else_sidecar`; `cargo test -p daemon lost_at_start`.
- [ ] **RB.2 The offer set and its persistence.** New `crates/daemon/src/resume_offer.rs`: `offer_ids(written, dead, entries, prev_last_line)` per Open question 1, `merge(pending, ids) -> StoredOffer`, `live_rows(offer, entries)` dropping recovered and pruned rows; `tracer_log::last_line_at(text)` for `daemon.log.old`; `PersistedState.resume_offer` with its getter and setter in `state.rs`; `main.rs` computes and stores it. Proof: `resume_offer::tests::*` (a written id is offered, a `TracerLost` 30 s before the old log's end is offered and one 10 min before is not, a recovered entry drops, a new start merges into a pending offer and keeps its id, no pending offer and no ids stays `None`), `state::tests::resume_offer_round_trips_and_defaults_to_none`; `cargo test -p daemon resume_offer`.
- [ ] **RB.3 Protocol.** `ResumeOffer`, `DaemonMessage::ResumeOffer`, `ClientMessage::AnswerResumeOffer`. File: `crates/protocol/src/lib.rs`. Proof: `resume_offer_round_trips` (with and without an offer), `answer_resume_offer_round_trips`; `cargo test -p protocol resume_offer` and `cargo test -p protocol v22_compat`.
- [ ] **RB.4 Daemon wiring.** `push_initial_state` sends the offer's live rows (`None` when empty); a `StateEvent::ResumeOffer` broadcast; `AnswerResumeOffer` handled in `dispatch`; `finish_recovery` and `finish_resume` clear the offer when its last row is recovered. File: `crates/daemon/src/server.rs`. Proof, on `spawnless_test_hub`: `resume_offer_sent_on_connect`, `answer_clears_offer_for_every_client`, `stale_offer_id_is_ignored`, `recovering_last_row_clears_offer`, `offer_survives_state_reload`; `cargo test -p daemon resume_offer`.
- [ ] **RB.5 Native model.** New `apps/native/src/resume_offer.rs`; `recover.rs` `default_items` and `RecoverDialog::for_offer`. Proof: unit tests: no prompt before the history arrives or while another modal is open; `resume_all` matches the dialog's defaults (known conversation, else newest; shell for a plain shell); a no-conversation row is left out and counted; `for_offer` ticks only offered rows; each action yields the right messages; `cargo test -p rustling-tulip-native --lib resume_offer` and `--lib recover`.
- [ ] **RB.6 Native view and routing.** New `apps/native/src/resume_offer_view.rs`; `lib.rs` mount; `net.rs` routing; `tests/support/mod.rs` fake-daemon `ResumeOffer` scripting. Proof: new `tests/ui_resume_offer.rs`: the prompt lists the offered sessions; Resume all sends `RecoverSessions` then `AnswerResumeOffer` and places the recovered sessions; Choose… opens the Recover dialog with only those rows ticked; Dismiss and Esc send only the answer and the Recover badge keeps its count; a broadcast `None` closes an open prompt; existing `ui_recover` stays green; `cargo test -p rustling-tulip-native --test ui_resume_offer --test ui_recover`.
- [ ] **RB.7 Live tier.** A new spec in `apps/native/tests/e2e_recover.rs`, `live_offers_resume_after_daemon_and_tracers_die`: spawn two fake-claude sessions, kill the daemon and both tracers with `kill_tree`, start the daemon again on the same `.tmp/` config, receive the offer naming both, Resume all, and see both respawned with `--resume` in their spawn lines; stop and restart the daemon before answering once to prove the offer persists. Proof: `pwsh ./rt.ps1 native-e2e` through the gate (heavy).
- [ ] **RB.8 Docs.** `docs/native-client.md` Session recovery (the prompt and its three buttons); `docs/architecture.md` (session history line and the task-index row: `resume_offer.rs` both sides); CLAUDE.md's `state.json` line (`resume_offer`); the README glossary (terms below, `RB.1` in the step-id entry); delete the MAIN.md line, borrowed-ideas.md's entry and this subplan once promoted.

## Glossary terms this subplan coins

- **Resume offer**: the daemon's pending list of sessions lost without an end at its last starts, persisted in `state.json` until a client answers the start prompt.
- **Lost at start**: a session whose sidecar the daemon finds dead at startup; it gets a `TracerLost` history entry if it had none.

## Open questions

Answered (user): Q1 (a) no end recorded plus tracer-lost ends within 60 s of the daemon's last log line; Q2 (a) any joint loss of the daemon and its tracers, no boot-time check; Q3 (a) a modal like the Recover dialog, Esc dismisses; Q4 (a) "Resume all" resumes the rows Recover would pre-tick and names the ones left in Recover.

1. **Which lost sessions the prompt offers.**
   - (a) Recommended: dead sidecars with no end recorded, plus dead sidecars whose unrecovered `TracerLost` entry ended within 60 s of the last line of `daemon.log.old`. How: covers a reboot where the dying daemon saw some tracers go first. Worst case: a tracer that crashed in the last minute before the user killed the daemon by hand is offered with the rest.
   - (b) Only dead sidecars with no end recorded (the ruling read literally). How: RB.2 offers exactly what RB.1 wrote. Worst case: in a reboot where the daemon outlives some tracers by a moment, those sessions are left out, so "Resume all" brings back only part of the set and the rest wait in Recover.
   - (c) Every dead sidecar with an unrecovered entry. How: no log reading. Worst case: a session lost days ago and left abandoned is offered again on every daemon start.
2. **A reboot only, or any loss of the daemon and its tracers together.**
   - (a) Recommended: any such loss (reboot, crash, `rt.ps1 stop`, killing the process tree). How: no boot-time check. Worst case: a developer who runs `rt.ps1 stop` against the real config dir gets the prompt on the next start.
   - (b) Only when the sessions started before the current boot (`sysinfo::System::boot_time()`). How: one more filter in `offer_ids`. Worst case: a crash that takes the daemon and every tracer down without a reboot leaves the sessions only in Recover, with no prompt.
3. **How the prompt appears.**
   - (a) Recommended: a modal centred over the window like the Recover dialog, shown once the session list, the history and the offer have arrived; Esc is Dismiss. Worst case: it stands between the user and the window on every start until answered.
   - (b) A banner across the top of the pane area with the three buttons, the rest of the window usable. Worst case: the user types into a pane, misses the banner, and it stays up until clicked.
4. **"Resume all" with rows it cannot resume on its own** (no conversation found, or a session known only by its folder).
   - (a) Recommended: resume the rows the Recover dialog would pre-tick, with its defaults; leave the others out and say so on the prompt ("2 can't be resumed automatically; they stay in Recover"). Worst case: a session resumes into the newest conversation in its folder, not its own, when its conversation id was never recorded.
   - (b) Show "Resume all" only when every row resumes into its own recorded conversation; otherwise only Choose… and Dismiss. Worst case: one row with no recorded id takes the one-click path away for the whole set.

## Findings outside this item

- `plan_recovery` recovers a Codex or Cursor entry that has a spawn config as a Claude session (`claude_request` swaps its `agent_options`), and its conversation candidates are whatever Claude transcripts the folder has in the session's time window, so the Recover dialog can offer a Codex session back as an unrelated Claude conversation. RB.5's defaults inherit this. Ruled (user): a Codex or Cursor entry recovers as a session of its own agent in the same folder (resumed when that CLI can resume, else a fresh run), never as a Claude transcript; tracked as its own MAIN.md line.
