# Bug hunt — running log

Open-ended audit of the daemon / tracer / frontend, started 2026-07-29 against
`c4c2676`. Baseline at start was green: `cargo clippy --all-targets
--all-features` clean, full workspace test suite passing, protocol Rust↔TS
mirror in sync (53 `DaemonMessage` + 81 `ClientMessage` tags, zero drift).
Everything recorded here is therefore *latent* — nothing the existing gates
catch.

This is a living document. Findings get appended as the sweep continues;
nothing is removed once written down, only ticked off or re-classified.

## How to read this

- `- [ ]` findings are unfixed. `- [x]` means fixed **and** covered by a
  regression test.
- Each finding carries a severity, a `file:line` anchor, and — where one was
  built — the repro that proves it. Repro sources live in the gitignored
  `.tmp/` and are disposable; the description is the durable part.
- "Confirmed" means a runnable probe reproduced it, not that it was reasoned
  about convincingly.

## Findings

### - [x] 1. `BracketedPasteTracker` drops carry context (High, confirmed)

`crates/daemon/src/termstate.rs:76-77`

```rust
let tail_start = chunk.len().saturating_sub(CARRYOVER);
self.carry = chunk[tail_start..].to_vec();
```

The carry is rebuilt from **the chunk**, discarding the previous carry that was
just folded into `window`. A DEC-2004 sequence spread across 3+ output chunks is
therefore missed — the second chunk's carry overwrites the one holding the `ESC`.

Repro (`.tmp/probe.rs`), replicating `observe` verbatim:

| case | result | expected |
|---|---|---|
| 3-way split (`ESC` \| `[?20` \| `04h`) | `enabled=false` | `true` |
| byte-at-a-time enable | `enabled=false` | `true` |
| 2-way split w/ padding | `enabled=true` | `true` |
| 2-way split, short first chunk | `enabled=true` | `true` |

The existing unit tests only exercise 2-way splits, which is why this passes CI.

Impact: the tracker never sees `\e[?2004h`, so the sidecar is never written and
the replay prefix is never added — i.e. the partial-paste bug that `c4c2676`
exists to fix comes back, silently.

Fix: carry the **window** tail, not the chunk tail.

```rust
let tail_start = window.len().saturating_sub(CARRYOVER);
self.carry = window[tail_start..].to_vec();
```

Verified to make all four cases pass. Cannot re-count an already-resolved
marker: a marker is 8 bytes and the carry holds at most 7, so a full marker can
never survive entirely inside the carry.

Note: `osc_title.rs` solves the same cross-chunk problem correctly, with a
persistent state machine. Worth considering whether `termstate` should follow
that shape rather than the carry-window one.

### - [x] 2. `classify_event` only excludes top-level build dirs (High, confirmed)

`crates/daemon/src/git_watch.rs:150-157` — `EXCLUDED_DIRS.contains(&first)`
tests only `comps.first()`, but both watchers register `RecursiveMode::Recursive`
(lines 530, 709).

Repro (`.tmp/gw.rs`):

```
node_modules\foo\index.js                      -> ignored
target\debug\build.rs                          -> ignored
apps\tauri-app\node_modules\react\index.js     -> WAKES git status
apps\tauri-app\dist\assets\main.js             -> WAKES git status
tools\e2e\node_modules\.pnpm\lock              -> WAKES git status
crates\daemon\target\debug\x.o                 -> WAKES git status
```

This is precisely the pathology the module docstring says the filter prevents
("anything that ran `cargo build` or wrote to `node_modules/` would keep firing
refreshes forever"), and it fires on *this* repo: `pnpm install` / `pnpm build`
under `apps/tauri-app` spawns a `git status` subprocess per 750 ms debounce
window for as long as the write traffic lasts.

The `.git` test on line 152 has the same first-component-only shape, so a
submodule's `.git/objects/` churn isn't filtered either.

Live confirmation on this checkout (2026-07-29): `apps/tauri-app/node_modules`,
`tools/e2e/node_modules`, and `apps/tauri-app/dist` all exist right now, so the
mis-classification is active, not hypothetical. Note the cost is the woken
watcher + spawned `git status` subprocess; the *output* of `git status` is
unchanged because those paths are gitignored, which is exactly why this has gone
unnoticed.

The test at `git_watch.rs:1012` only asserts top-level paths.

Fix: match every component against `EXCLUDED_DIRS`, and decide deliberately
whether nested `.git` should route to `classify_git_internal`.

### - [x] 3. Non-constant-time auth-token comparison (Low)

`server.rs:702` (`/shutdown`) and `server.rs:1142` (WS `Hello`) compare with
`!=`; `pairing.rs:100` correctly uses `constant_time_eq`. Both call sites are
served by the opt-in LAN TLS listener on `0.0.0.0` (`server.rs:589`), so they are
LAN-reachable when that feature is on.

Honest severity: a remote timing attack against `memcmp` over TLS is not
practically exploitable — network jitter swamps the signal. Logged because the
codebase already has the right primitive and applies it inconsistently, which is
the kind of gap that rots into a real one later.

### - [ ] 4. Replay fallback skips the bracketed-paste prefix (Minor)

`server.rs:3105` reads scrollback raw. Correct for orphan / abandoned / headless
sessions, but it is *also* the error path when the snapshot request times out or
the lifecycle task drops the reply (`server.rs:3040-3055`) — a **live** session
then gets an unprefixed replay, re-exposing finding 1's symptom.

Narrow: in that path the per-client forwarder isn't spawned either, so the
terminal is already broken and the missing prefix is the lesser problem.

### - [ ] 5. `expand_env_pattern` silently destroys text (Medium, confirmed)

`crates/daemon/src/presets.rs:656-673`. Two distinct defects in one function.
Applied to `LiteralPath` variables (`presets.rs:551`), `Toggle` values (`:562`),
and every Script arg (`:586` → `expand_script_token`). Free-form user prompt
text does **not** flow through it, which caps the blast radius.

Repro (`.tmp/env.rs`):

```
%Y-%m-%d                           -> "m-%d"
C:\out\file%20name.txt             -> "C:\out\file%20name.txt"   (single % is safe)
C:\out\%20a%20b.txt                -> "C:\out\20b.txt"
coverage 50% to 80% please         -> "coverage 50 please"
%APPDAT%\rustling-tulip            -> "\rustling-tulip"
${HOEM}/config                     -> "/config"
literal {a} and ${X} and %Y%       -> "literal {a} and  and "
```

**5a — paired literal `%` eats the text between them.** `%` is both the open and
close delimiter, so any two literal percent signs in one string are read as a
variable reference. A `-Format %Y-%m-%d` script arg becomes `m-%d`.

**5b — an unset variable silently expands to empty.** A typo'd `%APPDAT%\foo`
becomes `\foo`, a path that resolves to the drive root rather than failing. This
diverges from `cmd`, which leaves an undefined `%VAR%` verbatim.

The inconsistency is the tell: inside `expand_script_token`, an unknown `{name}`
is a **hard error** ("most likely a typo in the preset", `presets.rs:645-648`),
while an unknown `${VAR}` / `%VAR%` in the very same token is silent. Same typo
class, opposite handling.

Fix direction (needs a call — it's a behavior change for preset authors):
require `%VAR%` to look like an identifier before treating it as a reference, and
either error or preserve verbatim on unset, matching the `{name}` policy.

### - [x] 6. Stopping a headless session hangs and never kills the child (High, confirmed)

`crates/daemon/src/headless.rs:136-142` (exit waiter) vs `:39-46` (`kill`).

The exit waiter acquires the child mutex at spawn, `take()`s the `Child` out of
the `Option`, and then holds the guard across `child.wait().await` — i.e. for the
**entire lifetime of the process**:

```rust
let mut guard = handle_for_exit.child.lock().await;
let exit = if let Some(mut child) = guard.take() {
    child.wait().await.ok().and_then(|s| s.code())   // guard still held
} else { None };
```

`HeadlessHandle::kill()` needs that same mutex. Two compounding defects:

1. `kill()` blocks on `lock().await` until the child exits on its own.
2. Even once it gets in, the waiter already `take()`n the `Child`, so
   `guard.take()` yields `None` and the kill is a silent no-op.

Repro (`.tmp/hl/`, tokio, 3-second simulated child):

```
[t=0.20s] user clicks Stop -> stop_session().await h.kill()
[t=3.01s] exit waiter: child exited on its own
        >>> kill() got the lock, but Child was already taken -> NO-OP
[t=3.01s] kill() returned after blocking 2.81s
```

Both call sites `await` this inline in the WS message handler —
`stop_session` (`server.rs:4363`) and `park_session` (`server.rs:4103`) — so
Stop/Park on a running headless session blocks the handler for the child's full
remaining runtime and the child is never signalled.

No rescue path: `stop_session` computes
`had_live_handle = pty.is_some() || headless_handle.is_some()` (`server.rs:4358`),
so a headless session skips the pid-based `orphan::kill_pid` fallback at `:4365`
that would otherwise have saved it.

Only headless (`claude --print --output-format stream-json`) sessions are
affected; PTY sessions use `pty.kill()`, which is fire-and-forget and fine.

Fix direction: stop holding the guard across `wait()`. The idiomatic shape is a
`tokio::select!` in the waiter over `child.wait()` and a kill signal
(`oneshot`/`watch`), so `kill()` never contends for the child at all.
`HeadlessHandle` already stores `pid`, so a pid-based kill is an alternative.

### - [ ] 7. Secret-bearing files written with default permissions (Low today, blocks macOS)

No `set_permissions` call exists anywhere in the daemon for secret files —
`binary_cache.rs:175` is the only permission handling and it's about the
executable bit. Three secrets are written with `std::fs::write` defaults:

| file | secret | site |
|---|---|---|
| `lan-key.pem` | LAN TLS **private key** | `lan.rs:138` |
| `lan.json` | `auth_token` | `lan.rs:94` |
| `daemon.json` | `auth_token` + port | `server.rs:673` |

On Windows these inherit the `%APPDATA%` ACL, which is user-scoped — **not a
problem today**. On Unix, `std::fs::write` creates `0666 & ~umask`, i.e. usually
**0644, world-readable**: any local user could read the LAN private key and the
auth token.

This is filed because macOS compatibility is a greenlit, in-flight plan item
(`docs/plans/macos-compat.md`, Phase M0 next up). The port turns a non-issue into
a real local-privilege problem, so the fix (`0o600` via `PermissionsExt` on the
Unix arm) belongs in that plan rather than being discovered afterwards.

Minor sub-point, same area: `generate_and_persist_cert` (`lan.rs:137-138`) writes
cert then key non-atomically, unlike `state.json` / `meta.json` which both use
tmp+rename. A crash between the two self-heals (the `exists() && exists()` guard
regenerates), but a crash *during* the key write leaves both files present with a
truncated key — `load_persisted_cert` then fails forever and the LAN listener
never starts again without manual file deletion. Low probability, no self-heal.

### - [x] 8. Tracer version negotiation can never fail (Medium, confirmed)

`crates/daemon/src/tracer_client.rs:475-477`:

```rust
let negotiated = negotiate(welcome.version, &welcome.supported)
    .or_else(|| negotiate(welcome.version, SUPPORTED_TRACER_VERSIONS))
    .ok_or_else(|| anyhow!("no mutually supported tracer version"))?;
```

`negotiate(scalar, range)` (`tracer-protocol/src/lib.rs:199-207`) already
intersects *both* its arguments against `SUPPORTED_TRACER_VERSIONS`. So the
`.or_else` arm intersects `SUPPORTED_TRACER_VERSIONS` **with itself** — trivially
non-empty — and always returns the daemon's own maximum. The `ok_or_else` error
is unreachable.

Repro (`.tmp/tv.rs`, daemon hypothetically supporting `[2, 1]`):

```
compatible tracer (v2)             first=Some(2)  after .or_else=Some(2)
older but supported tracer (v1)    first=Some(1)  after .or_else=Some(1)
INCOMPATIBLE tracer (v7/v6 only)   first=None     after .or_else=Some(2)   <-- should be None
INCOMPATIBLE tracer, empty list    first=None     after .or_else=Some(2)   <-- should be None
```

Instead of a clean "no mutually supported tracer version" error, the daemon
proceeds to speak its own version at a tracer that doesn't understand it —
garbled frames rather than a diagnosable failure.

Latent today: `SUPPORTED_TRACER_VERSIONS == [TRACER_VERSION] == [1]`, so only one
version exists. But this guard exists precisely for reattaching to tracers left
running by an *older* daemon after an app upgrade, which is the one scenario an
ABI bump would produce — and it is broken before it ever gets used.

Fix: drop the `.or_else` arm. The first `negotiate` call is already the complete
intersection.

### - [ ] 9. Shell-integration command records grow without bound (Medium)

`apps/tauri-app/src/components/shellIntegration.ts:250` pushes an
`InternalRecord` per completed command; the array is only ever emptied at
`:294` (dispose). There is no cap, no ring, no eviction.

Each retained record holds the command text, two xterm markers, the decoration,
and — via `rec.dotEl = el` (`:161`) — a **reference to the chip's DOM element**.
xterm disposes the decoration and removes its element once the marker's line
scrolls out of the 5000-line scrollback, so the document self-cleans, but the
record keeps the now-detached node alive. The result is one detached DOM node
plus one record object per command, retained for the terminal's whole life.

This bites the app's core use case specifically: long-lived plain-shell sessions
that survive daemon restarts. A shell that runs thousands of commands over days
accumulates the lot. Slow leak, not a crash.

Fix direction: cap `records` (a ring bounded near the scrollback line count is
the natural fit) and null out `dotEl` when the decoration is disposed.

### - [ ] 10. OSC 133 `D` without an exit code is reported as failure (Low)

`shellIntegration.ts:234-246`. A bare `D` (no `;<code>`) parses to `NaN`, so
`exitCode` becomes `null`, and then:

```ts
current.status = current.exitCode === 0 ? "success" : "failure";
```

`null !== 0`, so the chip renders red. Per OSC 133, an unparameterized `D` means
the command finished with **unknown** status, not a failed one — VS Code's
integration distinguishes the two.

Not reachable from the daemon's own hooks: `bash-init.sh:43` and
`zsh-init.zshrc:34` both always emit `133;D;%s`, and the PowerShell wrapper
guards with `if ($null -ne $rt_exit)` (`server.rs:4863`). It only shows up when a
user's own rc already carries shell integration, or a program inside the terminal
emits its own marks. Display-only.

Fix: give `status` a third state for `exitCode === null` rather than folding it
into `failure`.

### - [ ] 11. Scrollback request gives up before the daemon does (Medium)

`apps/tauri-app/src/api.ts:388` times out `loadScrollback` after **2 s**;
`server.rs:3013` sets `SCROLLBACK_SNAPSHOT_TIMEOUT` to **5 s**. The frontend can
therefore never observe the daemon's own timeout, and any reply arriving between
2 s and 5 s is dropped with no listener attached.

On timeout `loadScrollback` resolves `null`, and `Terminal.tsx:657-663` does:

```ts
const sb = await loadScrollback(client, sessionId);
if (sb && sb.data_b64.length > 0) { … term.write(…) }
```

`null` writes nothing — no history, no warning, no "couldn't load history" line.
The user sees a blank terminal that is indistinguishable from a session with no
scrollback. Plausible to hit on a burst attach (the 9-session preset launch that
`inject.rs` comments already cite as a load case) where the lifecycle task has to
drain its output channel before answering, or on a 2 MB ring read.

The codebase already learned this exact lesson one function away — `listPresets`
(`api.ts:448-452`) documents: *"Returns a discriminated `{ ok: true } | { ok:
false }` so the caller can distinguish 'fetched, none defined' from 'request
failed / timed out'. Previously both fell through to `entries: []`, which left
the context menu stuck on '(loading)'."* `loadScrollback` still has the
undiscriminated shape that fix was written to remove.

Fix: raise the client timeout above the daemon's, and return a discriminated
result so `Terminal.tsx` can surface a timeout distinctly from empty history.

### - [ ] 12. Silently swallowed errors — four outlier sites (Low)

CLAUDE.md's rule is "never swallow exceptions silently — at minimum, log". Four
sites break it, and each is an outlier against its own neighbours:

| site | swallowed | consequence |
|---|---|---|
| `server.rs:1580` | `git::list_branches` error | branch dropdown silently empty |
| `server.rs:1581` | `git::current_branch` error | no current-branch highlight |
| `server.rs:1599` | `git::list_worktrees` error | worktree list silently empty |
| `registry.rs:115` | `state.mutate` **persist failure** | default branch silently not saved |

The first three `.unwrap_or_default()` / `.unwrap_or(None)` with no `warn!` at
all, so a broken repo, a missing `git` on PATH, or a held `index.lock` renders as
"this repo has no branches" — a real state, indistinguishable from the error.

`registry.rs:115` is the starkest: it is the **only one of ~23 `state.mutate`
call sites** that discards the `Result`. Every sibling in that same file
propagates with `?`. Impact is mild (the value is re-detected next time) but a
failing state.json write deserves a log line.

## Fragilities (not bugs — undocumented couplings worth a comment)

- `MAX_PTY_INPUT_CHUNK_BYTES` (2048, `pty.rs:15`) must stay **greater than**
  `PACING_CHUNK_BYTES` (1024, `tracer/src/supervisor.rs`) or the tracer's paste
  pacing silently stops engaging — every frame would arrive at or under the
  threshold and take the unpaced branch, resurrecting the ConPTY overrun that
  `10edf83` fixed. Neither constant references the other.

- **Client-supplied `job_id` is used as a server-side map key.**
  `presets.rs:119-126` only mints a UUID when the id is *empty*; any non-empty
  client string is taken verbatim and `insert`ed into `hub.preset_cancellations`
  (`presets.rs:85`). Two concurrent launches sharing an id would silently evict
  each other's cancel sender, and whichever finished first would `remove` the
  other's. Not live today — the frontend mints ids via `newPresetLaunchJobId()`
  — but the daemon supports multiple simultaneous clients (main window,
  pop-outs, a remote laptop) and trusts them here.

- **mDNS discovery is TOFU over an unauthenticated channel.** The fingerprint a
  discovering client pins comes from the mDNS TXT record, which any host on the
  LAN can advertise. An attacker advertising a look-alike service could receive
  the pairing code the user reads off their real desktop and hand back their own
  token, leaving the user driving the attacker's daemon. Inherent to the
  documented TOFU model and not a defect in the implementation — the pasted
  connection-code path is unaffected because its fingerprint arrives
  out-of-band. Noting it only because the UI may be worth checking for whether
  it distinguishes the two provenances.

- **One throwing message callback stops the rest.** `api.ts:321` runs
  `for (const cb of messageCbs) cb(parsed)` with no per-callback guard, so an
  exception in one subscriber skips every later subscriber for that frame and
  escapes into the WebSocket event handler.

## Audited and found clean

Recorded so a later pass doesn't redo the work:

- Every `saturating_sub`-based string slice (`inject.rs:320`, `inject.rs:334`,
  `pty_state.rs:307`, `presets.rs:775`). Both `strip_ansi` implementations push
  ASCII only, so all byte offsets are char boundaries; `presets.rs` explicitly
  walks forward to a boundary. No panic risk.
- `osc_title.rs` escape-scan loops: continuation bytes (0x80-0xBF) can never
  match `BEL`, `ESC`, or a CSI final byte (0x40-0x7E), so byte-wise scanning
  always exits on a char boundary.
- Grid/tab tree manipulation — `tabs.rs` (`split_pane`, `close_pane`,
  `remove_pane_in_split`, `set_pane_ratio`) and `utils/grid.ts`.
- `binary_cache::gc` retain-set logic.
- Protocol Rust↔TS tag mirror (no drift).
- Input-pacing chain arithmetic (pacing does engage today — see Fragilities).
- Zero `TODO` / `FIXME` / `HACK` / `XXX` anywhere in `crates/` or the frontend,
  and no skipped/`.only` e2e specs. Unusually clean.

## Sweep coverage

Areas visited so far, so the next pass can pick up where this left off.

- [x] `termstate.rs` (new in `c4c2676`)
- [x] `scrollback.rs`, `orphan.rs` sidecar lifecycle
- [x] `git_watch.rs` event classification
- [x] `pty.rs` / `tracer/supervisor.rs` input pacing
- [x] `osc_title.rs` parser
- [x] `tabs.rs` + `utils/grid.ts` layout trees
- [x] Protocol mirror drift
- [x] String-slice panic surface (daemon-wide grep)
- [x] `inject.rs` injector runner
- [x] `binary_cache.rs`
- [x] `presets.rs` template rendering + variable substitution → finding 5
- [x] `headless.rs` child lifecycle → finding 6
- [x] `worktrees_admin.rs` delete guards (`validate_target` is solid:
      canonicalize + under-root + `wt.` prefix + live-session check)
- [x] `state.rs` persistence (atomic tmp+rename, lock held across the write —
      no torn-file window)
- [x] Protocol version negotiation + `daemon_supervisor` retire path (the
      singleton `"supported": [21]` in `protocol-version.json` is safe: the
      `Incompatible` → `retire_daemon` → graceful `/shutdown` → wait → force-kill
      chain is fully wired)
- [x] `tracer/src/ring.rs` output ring
- [x] `App.tsx` `subscribePty` listener-map lifecycle (correct add/remove +
      empty-set cleanup)
- [x] Frontend gates: `tsc --noEmit` clean, `vitest` 15/15 pass
- [x] `pairing.rs` — TTL, attempt cap, single-use window, constant-time compare
      all correct; `/pair` holds the lock across `validate` so the attempt
      counter can't be raced
- [x] `remote.rs` pinned-TLS verifier — correctly delegates
      `verify_tls12_signature` / `verify_tls13_signature` to rustls rather than
      stubbing them (the usual way custom verifiers get this wrong); bridge
      binds loopback only
- [x] `lan.rs` cert generation → finding 7
- [x] `tracer_client.rs` reattach + handshake → finding 8
- [x] `shellIntegration.ts` OSC 133/633 parser → findings 9, 10
- [x] `agents/claude.rs` headless stream-json parser
- [x] `git_inspect.rs` porcelain parsing (`entry[3..]` is boundary-safe — the
      status bytes are always ASCII and `len >= 3` is checked)
- [x] `close_session_panes` / `rebind_session_panes` per-client layout fan-out
- [x] `branch_slug` / `sanitize_anchor` — no path traversal reachable; git's own
      refname rules reject the characters that would matter
- [x] `worktree_cleanup.rs` — retry ladder + `prune_empty_ancestors` (uses
      non-recursive `remove_dir` and is bounded by `cur != root &&
      starts_with(root)`; a non-empty sibling correctly stops the walk)
- [x] `git_write.rs` — every subcommand puts `--` before paths (no pathspec
      injection); `discard` uses `clean -fd` **without** `-x`, so ignored files
      survive; `partition_tracked` uses `-z` so C-quoting can't corrupt matching
- [x] `tracer/job_object.rs` — RAII on both handles, correct
      `KILL_ON_JOB_CLOSE`; the `OpenProcess(pid)` TOCTOU is not reachable because
      portable-pty holds the child handle, and Windows won't recycle a PID while
      a handle is open
- [x] `lock_finder/windows_impl.rs` — `RmGetList` retry loop and
      `wide_to_string` NUL handling are correct
- [x] `daemon/main.rs` orphan-tracer reap — now scoped to `dirs.binaries_dir`,
      which closes the "probe daemon reaps the installed app's tracers" hazard
- [x] `autostart.rs` — HKCU `Run` value is quoted; the macOS LaunchAgent plist
      XML-escapes the program path with `&` replaced first
- [x] `registry.rs`, `workspace.rs`, `vscode.rs`, `paths.rs`, `agents/*`,
      `protocol/build.rs`, `tracer/main.rs`, `apps/.../lib.rs` — risk-pattern
      scan (string slicing, swallowed errors, command construction) → finding 12
- [x] `server.rs` — handler sweep via pattern scan (79 fns): `let _ =` audit,
      `unwrap_or` audit, per-client layout fan-out, `/pair` + `/shutdown`
      handlers, handshake, `close_session_panes` → findings 6, 11, 12
- [x] Frontend: no `!` non-null assertions, no `as any`, no `@ts-ignore`
      anywhere; every `JSON.parse` guarded; listener/timer add-remove balance
      audited across all `.ts`/`.tsx` (the 4 unbalanced sites are all
      intentionally process-lifetime)
- [x] `api.ts` one-shot request helpers — all pair a timeout with a `cleanup()`
      that removes the listener on both paths → finding 11

### Deliberately not swept

- `Sidebar.tsx` / `SpawnDialog.tsx` / `PresetLaunchDialog.tsx` /
  `SourceControlSidebar.tsx` internals — presentational React reviewed only by
  pattern scan, not read line-by-line. Bugs here would be visual/interaction
  issues that hand-testing finds faster than reading does.
- `workspace.rs` / `git.rs` cross-drive anchor fallback — the order-dependence
  (which member lands on the anchor depends on `parents[0]`) is real but is
  already documented as lossy-by-design for cross-drive workspaces.
- e2e suite not executed (needs a display + tauri-driver session).
