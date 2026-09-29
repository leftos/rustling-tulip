# Link a session to a GitHub issue or PR

Design for the MAIN.md line "Link a worktree to a GitHub issue or PR at spawn, through `gh` only". Origin: [borrowed-ideas.md](./borrowed-ideas.md), "Link a worktree to an issue or PR". Chip style: [petal.md](./petal.md) (PT.6b leaves, PT.7 pane-header chips, PT.8b spawn dialog); petal.md already routes its `#412` / `#1187 draft` chips to this item. Step ids are `GL.n`.

## Problem

A worktree session is usually about one GitHub issue or one pull request, but nothing in the app records which. The user keeps the mapping in their head or in the branch name, opens the browser to see whether the PR exists or merged, and a leaf reading `wt/amber-otter` says nothing about the work in it.

## Rulings

- Ruling (user): GitHub only, through `gh`; no provider abstraction.
- Additive protocol only: no version bump, protocol 22 keeps decoding (`cargo test -p protocol v22_compat`).
- Testable core, thin view: parsing, lookup state, chip text and PR-state logic live in plain-Rust modules with unit tests; GPUI views only render.

## Design

### What the code does today (measured)

- `SpawnRequest` (`crates/protocol/src/lib.rs`) carries no link. `SpawnConfig::from_request` captures the replayable subset; it is stored in `meta.json` (`orphan::meta_from_record`), `history/<id>.json` (`HistoryEntry.spawn_config`) and `state.json` (`last_spawn_config`), and replayed by `to_clone_request` (Restart, `resume_abandoned`, Recover's `history::claude_request`, Launch last) and `to_duplicate_request` (`server::duplicate_session`). `SpawnConfig` has a hand-written `Deserialize` through a `Helper` struct, so a new field is added there too.
- `SessionSnapshot` derives spawn-config facts in `SessionRecord::snapshot` (`crates/daemon/src/session.rs`), as `elevated_authority` does.
- No code runs `gh` today. Git runs through `git.rs::run_git_inner`: `Command::new("git")`, `stdin(null)`, piped output, `CREATE_NO_WINDOW` on Windows, `NON_INTERACTIVE_ENV` plus `kill_on_drop` for network calls, begin/slow timing logs. `git_inspect::parse_forge` already maps an origin URL to `https://github.com/<owner>/<repo>` and forge `github`.
- `git_watch.rs` watches `.git/refs/**` per repo and parks while `Hub.client_count` is 0.
- Native: `PaneHeaderParts.chips` (`grid_view.rs` `header_parts`) is a list of `(text, tip)`; `sidebar::Leaf` has `runtime`, `trusted` and `state` tags; `apps/native/src/open.rs` opens a URL through `ShellExecuteExW`; `combobox.rs` is the branch picker's pure list state.

### Protocol (additive)

- `IssueLink { kind: LinkKind, owner: String, repo: String, number: u64, url: String, title: String }`, `LinkKind { Issue, PullRequest, #[serde(other)] Unknown }`.
- `PrInfo { number: u64, url: String, title: String, state: PrState }`, `PrState { Open, Draft, Merged, Closed, #[serde(other)] Unknown }`.
- `SpawnRequest.link: Option<IssueLink>` and `SpawnConfig.link: Option<IssueLink>`, both `#[serde(default, skip_serializing_if = "Option::is_none")]`, so unlinked spawns and stored configs stay byte-identical; `from_request` and `to_clone_request` copy it; `to_duplicate_request` follows Open question 6.
- `SessionSnapshot.link: Option<IssueLink>` (from `spawn_config`) and `SessionSnapshot.pr: Option<PrInfo>` (the daemon's last lookup), both `#[serde(default)]`.
- `ClientMessage::LookupIssues { request_id, target: SuggestTarget, query: String }` → `DaemonMessage::IssueLookup { request_id, outcome: LookupOutcome }`, `LookupOutcome { Found { items: Vec<IssueLink> }, NotGithub, GhMissing, GhUnauthenticated, Failed { message }, #[serde(other)] Unknown }`. A `#123`, `owner/repo#123` or `https://github.com/o/r/(issues|pull)/123` query resolves one item; other text searches open issues and PRs of the target's repo.
- Protocol 22: the Tauri app ignores the new fields and messages. Its Launch last re-sends a decoded `SpawnConfig` without the link; its Duplicate, Restart and Resume go through the daemon and keep it. Stated, not fixed (the Tauri app is being retired).

### Daemon

- New `crates/daemon/src/gh.rs`:
  - `github_repo(remote_url) -> Option<(owner, repo)>`, built on `parse_forge` (made `pub(crate)` in `git_inspect.rs`, no second parser).
  - `Query::parse(&str) -> Query { Ref { owner?, repo?, number, kind? }, Text(String) }`.
  - Argument builders: `gh issue view <n> -R o/r --json number,title,url,state`, `gh pr view <n> -R o/r --json number,title,url,state,isDraft`, `gh search issues <text> --repo o/r --include-prs --json number,title,url,isPullRequest --limit 20`, and for PR state `gh pr list -R o/r --head <branch> --state all --json number,title,url,state,isDraft --limit 1`; parsers from `gh`'s JSON to protocol types.
  - `classify(exit, stderr)`: program not found → `GhMissing`; `gh auth login` in stderr → `GhUnauthenticated`; else `Failed` with the stderr's first line.
  - Runner: `Command::new(gh_program())` (`RUSTLING_TULIP_GH`, else `gh` on PATH, for a fake in tests), `stdin(null)`, `GH_PROMPT_DISABLED=1`, `NO_COLOR=1`, `CREATE_NO_WINDOW`, `kill_on_drop`, a 15 s timeout, begin/slow logs as `run_git_inner` has. Behind a `GhRunner` trait so tests inject canned output.
- `server.rs`: `LookupIssues` resolves the target's repo (a workspace: Open question 5), reads its origin URL, returns `NotGithub` when `github_repo` fails, else runs the query off the message loop and replies to the requester only.
- Spawn: `spawn_session` stores the link in `SpawnConfig` as given; the daemon does not re-fetch it (the client picked it from a lookup, and a spawn must not wait on the network). A link whose `owner/repo` matches none of the session's remotes is still accepted.
- New `crates/daemon/src/pr_watch.rs`: per live linked session, the PR state; Open question 3 sets who and how often (recommended below). A change sets `SessionRecord.pr` through `update_from`, which broadcasts `SessionUpdated`. `pr` is not persisted: after a daemon restart the first poll fills it.

### Native client

- New `apps/native/src/issue_link.rs` (plain Rust): `LinkField` (query text, 300 ms debounce, the latest `request_id`, results, highlighted row, chosen link, the last outcome's message: "Not a GitHub repo", "`gh` is not installed", "Run `gh auth login`"); `chip(link, pr) -> Option<LinkChip { text, tip, url }>`: `#412` for an issue with no PR, `#1187 draft` / `#1187 open` / `#1187 merged` / `#1187 closed` for a PR (its own or the one found for the branch), the tip "<owner>/<repo>#<n> <title>\n<state>\n<url>".
- Spawn dialog (`spawn_form.rs`, `spawn_view.rs`): a "Link" field under Branch for repo and workspace targets, hidden for standalone shells, in PT.8b's field layout, with a results list drawn like the branch combobox. Picking a row shows the link as a chip with a clear button; `build_request` sends `link`. Launch last and the Shift-duplicate prefill carry it per Open question 6. `net.rs` routes `IssueLookup` to the open form by `request_id`, dropping stale replies.
- Chips, Petal style (20 px, 5 px radius, Geist Mono 11 px at 400, `CHIP` ground, `TEXT_2` text; no new token, PT.2 froze `palette.rs`): on the pane header after the runtime and trusted chips (`header_parts`), and in the leaf's chip row (Comfortable) or inline (Compact). A click opens the URL through `open.rs`; the leaf and header tooltips carry the tip. An `Unknown` kind or state shows `#<n>` with no state word.

## Steps

Order: GL.1 → GL.2 → GL.3 → GL.4 → GL.5; GL.6 needs GL.1; GL.7 needs GL.3, GL.6 and PT.8b; GL.8 needs GL.5, GL.6, PT.6b and PT.7; GL.9 follows the rest. Gates are MAIN.md's.

- [ ] **GL.1 Protocol types.** `IssueLink`, `LinkKind`, `PrInfo`, `PrState`, `SpawnRequest.link`, `SpawnConfig.link` (with the `Helper` field, `from_request`, `to_clone_request`, `to_duplicate_request`), `SessionSnapshot.link` / `.pr`, `LookupIssues`, `IssueLookup`, `LookupOutcome`. Files: `crates/protocol/src/lib.rs`. Proof, red first: new tests `issue_link_round_trips`, `unlinked_spawn_request_has_no_link_key`, `unknown_link_kind_and_pr_state_decode`, `spawn_config_carries_link_through_clone`, `lookup_issues_round_trips`; `cargo test -p protocol` (includes `v22_compat`).
- [ ] **GL.2 `gh` module.** New `crates/daemon/src/gh.rs` (`github_repo`, `Query::parse`, argument builders, JSON parsers, `classify`, `GhRunner` and the process runner), its `mod` line in `main.rs`, `parse_forge` made `pub(crate)`. Proof: unit tests `query_parses_hash_number_owner_repo_and_urls`, `non_github_remote_is_none`, `parses_issue_pr_search_and_pr_list_json`, `classify_missing_and_unauthenticated`, `runner_sets_prompt_disabled_env`; `cargo test -p daemon gh::`.
- [ ] **GL.3 Lookup handler.** `LookupIssues` in `server.rs` over a `GhRunner` on the hub (the process runner in production, a fake in tests). Files: `crates/daemon/src/server.rs`. Proof: `test_hub` tests `lookup_by_url_replies_found_to_requester_only`, `lookup_on_gitlab_remote_replies_not_github`, `lookup_with_gh_missing_replies_gh_missing`; `cargo test -p daemon lookup_`.
- [ ] **GL.4 Link on spawn and carry-over.** `spawn_session` stores the link; `SessionRecord::snapshot` derives `link`; carry-over tests for Restart, `resume_abandoned`, Recover's `claude_request`, Duplicate (per Open question 6) and `last_spawn_config`. Files: `crates/daemon/src/session.rs`, `server.rs`, `history.rs` (tests). Proof: `linked_spawn_snapshots_link`, `resume_and_recover_keep_link`, `duplicate_link_follows_ruling`; `cargo test -p daemon`.
- [ ] **GL.5 PR state watcher.** New `crates/daemon/src/pr_watch.rs`: the schedule from Open question 3, parking at `client_count == 0`, a PR link polled with `gh pr view`, an issue link with `gh pr list --head <branch>` per Open question 7, `gh` failures logged once per session and state and never broadcast as errors. Files: `pr_watch.rs`, `main.rs`, `server.rs` (start and stop per session), `session.rs` (`pr` on the record). Proof: unit tests with a fake runner and a paused tokio clock: `pr_state_change_broadcasts_session_updated`, `issue_link_finds_branch_pr`, `watcher_parks_with_no_clients`, `unlinked_session_never_polls`; `cargo test -p daemon pr_watch`.
- [ ] **GL.6 Native model.** New `apps/native/src/issue_link.rs` (`LinkField`, `chip`); `spawn_form.rs` holds a `LinkField`, sends `link` from `build_request`, takes it from a `last_spawn_config` prefill. Proof: unit tests `chip_text_per_kind_and_state`, `stale_lookup_reply_is_dropped`, `debounce_sends_one_lookup`, `outcome_messages`, `build_request_sends_chosen_link`, `standalone_target_has_no_link`; `cargo test -p rustling-tulip-native --lib`.
- [ ] **GL.7 Spawn dialog field.** `spawn_view.rs` draws the Link field, its results list and the chosen chip; `net.rs` routes `IssueLookup`; `tests/support/mod.rs` scripts `IssueLookup` replies. Files: `spawn_view.rs`, `net.rs`, `tests/support/mod.rs`, `tests/ui_spawn_dialog.rs`. Proof: new specs in `ui_spawn_dialog`: `link_field_lookup_and_pick_sends_link`, `link_field_shows_gh_unauthenticated_message`, `link_field_hidden_for_plain_shell_folder`; `cargo test -p rustling-tulip-native --test ui_spawn_dialog`.
- [ ] **GL.8 Chips on the leaf and the pane header.** `header_parts` adds the link chip with its URL; `sidebar::Leaf` gains `link: Option<LinkChip>`; `sidebar_view.rs` and `grid_view.rs` draw it and open the URL on click; `SessionBuilder::link` / `::pr`. Files: `grid_view.rs`, `sidebar.rs`, `sidebar_view.rs`, `tests/support/mod.rs`, `tests/ui_panes.rs`, `tests/ui_sidebar.rs`. Proof: `ui_panes` `pane_header_shows_pr_chip`, `ui_sidebar` `leaf_shows_link_chip_and_updates_on_pr_state`; `cargo test -p rustling-tulip-native --test ui_panes --test ui_sidebar`; hand-test the chip at the narrowest sidebar width.
- [ ] **GL.9 Docs.** `docs/architecture.md` (Sessions line; task-index row "Change issue and PR links": `gh.rs`, `pr_watch.rs`, `issue_link.rs`), `docs/native-client.md` (the Link field, the chip), CLAUDE.md's environment table (`RUSTLING_TULIP_GH`), the README glossary (terms below, `GL.1` in the step-id entry); delete the MAIN.md line, borrowed-ideas.md's entry and this subplan once promoted. Proof: the diff.

## Glossary terms this subplan coins

- **Issue link**: the GitHub issue or pull request a session was spawned for, stored in its spawn config and shown as a chip.
- **PR state**: the daemon's last-fetched state (open, draft, merged, closed) of a linked PR, or of the PR found for an issue-linked session's branch.

## Open questions

1. **Per session or per worktree/branch.**
   - (a) Recommended: per session, in `SpawnConfig`, so every respawn path carries it with no new store. Worst case: a second session launched into the same worktree without a link shows none, though the branch is the same work.
   - (b) Per worktree branch, in `state.json` keyed by `(repo_id, branch)`, shown on every session on that branch. Worst case: a new store with its own cleanup when the branch is deleted, and a reused branch name inherits a stale link.
2. **Search UX in the spawn dialog.**
   - (a) Recommended: one Link field; a number, `owner/repo#n` or URL resolves one item, other text searches the repo's open issues and PRs, debounced, results in a combobox list. Worst case: a slow `gh search` leaves the list empty for a few seconds.
   - (b) Paste only (URL or number), no search. Worst case: the user leaves the app to find the number.
   - (c) A list of the repo's open issues and PRs shown when the field opens, filtered as they type. Worst case: a `gh` call per dialog open, and large repos show only the first page.
3. **Who polls PR state, and how often.**
   - (a) Recommended: the daemon, per live linked session, every 5 minutes while a client is connected, plus once when the session's branch ref changes under `.git/refs/remotes/` (a push) and once at client reconnect. Worst case: a PR opened from the browser shows up to 5 minutes late.
   - (b) The daemon, every 60 s. Worst case: with twenty linked sessions, twenty `gh` calls a minute against GitHub's rate limit.
   - (c) The client, when a leaf or header is drawn, cached. Worst case: each connected client polls separately and headless/remote clients get no state.
4. **`gh` missing or unauthenticated.**
   - (a) Recommended: the Link field shows the outcome's message ("`gh` is not installed", "Run `gh auth login`") and the spawn still works without a link; the watcher logs once and keeps the chip without state. Worst case: a user without `gh` sees a field that never works until they read the message.
   - (b) Hide the Link field unless a startup `gh auth status` succeeded. Worst case: a user who logs in later must restart the daemon to see the field.
5. **Remotes not on GitHub, and workspaces.**
   - (a) Recommended: search uses `origin` of the single repo, or of `members[0]` for a workspace; a pasted URL or `owner/repo#n` works for any GitHub repo; a non-GitHub origin replies `NotGithub` and only pasted GitHub URLs resolve. Worst case: in a workspace, searching for another member's issue needs `owner/repo#n`.
   - (b) Workspace search runs across every GitHub member. Worst case: one `gh` call per member per keystroke burst.
6. **Carry-over through Duplicate, Recover, Restart, Resume and Launch last.**
   - (a) Recommended: Restart, Resume, Recover and Launch last keep the link (they continue the same work); Duplicate keeps it too, since a duplicate is a second attempt at the same issue, and the prefill shows it with a clear button. Worst case: a duplicate of a PR-linked session shows the original PR's chip on a branch that is not that PR's.
   - (b) As (a), but Duplicate drops a PR link and keeps an issue link. Worst case: the user re-links a PR by hand when they really meant a second session on it.
7. **Which PR an issue-linked session shows.**
   - (a) Recommended: the PR whose head is the session's branch (`gh pr list --head <branch> --state all`, newest). Worst case: a PR pushed from another branch name for the same issue is not found.
   - (b) The PRs GitHub lists as closing the issue (`closedByPullRequestsReferences`). Worst case: only PRs that say "Fixes #n" count, and several can match.
8. **Changing the link after spawn.**
   - (a) Recommended: not in this item; a link is chosen at spawn, as the ruling says. Worst case: a session spawned without one stays unlinked until respawned.
   - (b) "Link issue or PR…" in the session menu, a new `SetSessionLink` message that rewrites `spawn_config` in `meta.json`. Worst case: one more message and a sidecar write path.
9. **Telling the agent about the link.**
   - (a) Recommended: no; the link is metadata for the user, and the prompt stays what they type. Worst case: the user pastes the issue into the prompt as well.
   - (b) Prefill an empty interactive prompt with "GitHub issue <owner>/<repo>#<n>", as the GitHub-issues preset source does. Worst case: an unwanted kickoff message the user must delete.
