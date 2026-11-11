# Agent plugins: one module per agent behind one seam

Every agent-specific decision lives in that agent's crate. The app (daemon, protocol, native client) asks a registered plugin for what it needs through one trait and one wire schema, and never names an agent. Claude is the first plugin; Codex and Cursor return as further plugins on the same seam ([plans/agent-harnesses.md](./plans/agent-harnesses.md)). `tools/check-agent-names.ps1` enforces the seam (see Testing).

## Rulings

- An agent's UI comes from the daemon as a schema: each agent module declares its options (toggles, choices, text, model lists) as typed descriptors, the daemon sends them to clients, and the client renders Settings pages, spawn-dialog options and chips generically, with no per-agent client code. Remote and mobile clients get a new agent without a client release.
- Plugins are compiled in, one crate each, each implementing one trait; the registry lists them. No runtime loader.
- Nothing outside an agent's module branches on that agent.
- Saved user data survives every update (CLAUDE.md, "Saved user data survives every update"): a wire change follows the protocol bump rules, and a `state.json`, preset or history shape change reads the last release's shape.
- An agent's Settings values live on the daemon, in `state.json` per agent, pushed to every client.
- DeepSeek routing is a Claude plugin option: a Provider choice in Claude's schema; only the Claude crate knows the environment mapping, and the app only looks keys up.
- The plain shell is the app's built-in baseline, not a plugin: `SessionMode::PlainShell`, `spawn_plain_shell_session` (`crates/daemon/src/server/spawn.rs`), `resolve_shell_program`, `pty_state::watch_plain_shell` and the client's `Runtime::PlainShell` (`apps/native/src/spawn_form.rs`) stay app code. A plain shell has no agent of its own.
- The trait is synchronous and complete enough for Codex and Cursor to return with no app change: argv-injected hooks, pre-created and discovered conversation ids, per-agent passthrough variables, regex screen rules, a startup ready signal and a prompt file-reference syntax are plugin capabilities, and the app runs every process, poller and timeout.
- Options are typed inside each plugin. A value map travels on the wire and on disk, validated by the app's one schema function; each plugin works with its own `Options` struct, adapted by `agent-api`, and a conformance test runs every descriptor through its struct.
- Dependent fields are declarative `when` rules on a field, evaluated by the client and applied by the daemon too (a value a rule hides is dropped daemon-side).
- `Welcome.features` is for app-level capabilities only. `model` is a field of the agent's schema.

## Scope

Inside the seam (the plugin owns it): the CLI's binary name and override variable; its argv for interactive and headless runs; its environment routing (DeepSeek) and the variables it needs passed through; its hook files, hook arguments and hook vocabulary; its screen prompt rules; how its conversation id is chosen or found, its transcripts and resume flags; its recovery choices and shell-typed resume command; its headless stream parser; its option schema and chips; its startup ready signal and prompt file-reference syntax; its tracer-log import; its fake CLI for tests.

Outside the seam (app code, agent-neutral): the plain shell; the PTY, `rt-tracer.exe`, the tracer ABI and reattach; scrollback; tabs and layouts; worktrees, workspaces and git. The hook side the app keeps is the `/hook/{session_id}` route, the hook ordering stamp, the early-event buffer and `apply_status` (`crates/daemon/src/hook_status.rs`, `pty_state.rs`), plus the idle demotions and the byte-volume and title heuristics.

Also app code: env-row sealing and `${env:}` / `${secret:}` resolution; the base environment keep-list (`crates/daemon/src/child_env.rs`); npm-shim resolution (`resolve_agent_program`, `server/spawn/shell.rs`); the deprecated `dangerously_skip_permissions` wire flag, folded into the descriptor's `bypass` choice (`crates/daemon/src/trusted_fold.rs`); every process the app runs for a plugin (a pre-create, a discovery poll) with its timeout and kill; history storage, pruning and the recover dialog's layout; the schema validation function.

## Crate layout

```
crates/agents/api/       package `agent-api`: the typed `Agent` trait, the object-safe `AgentPlugin`, context types, the schema validator, the shared hook-vocabulary helper
crates/agents/claude/    package `agent-claude`: the Claude plugin (argv, DeepSeek routing, hooks, rules, transcripts, schema, fake CLI)
crates/agents/registry/  package `agent-registry`: the one list of registered plugins and lookup by id
crates/protocol/         the wire half of the schema (AgentId, AgentSelection, AgentDescriptor, OptionField, OptionValues)
```

Dependency rules:

- `protocol` depends on no agent crate. The schema types are wire types, so they live in `protocol`.
- `agent-api` depends on `protocol`, `serde`, `serde_json`, `chrono` and `tracing` only: never on `daemon`, `tracer`, `daemon-client` or `apps/native`.
- An agent crate depends on `agent-api` (which re-exports the protocol types it uses) and its own parsing needs; never on the daemon, the client or another agent crate.
- `agent-registry` depends on `agent-api` and every agent crate, and holds the registered list, `plugin(&AgentId) -> Option<&'static dyn AgentPlugin>`, `all()`, `descriptors()` and `default_agent()` (the first registered). It is its own crate because the daemon is bin-only and the native client's live tests need the same list to point each plugin's override at its fake CLI (a dev-dependency of `apps/native`, never a normal one).
- `daemon` depends on `agent-api` and `agent-registry` only. `apps/native` depends on `protocol` only in its normal build: everything it knows about an agent arrives in the descriptor list.
- `agent-registry`'s `lib.rs` and `Cargo.toml` are the only files outside an agent crate that name one; the seam check allows exactly those (and `Cargo.lock`).

A request naming an id the registry lacks is refused ("This daemon has no agent `x`."). A preset that names no agent runs `default_agent()`.

## The trait

A plugin implements the typed trait; `agent-api` adapts every implementor to the object-safe `AgentPlugin` the registry holds, decoding the validated `OptionValues` into `Self::Options` once per call. Plugins are stateless statics. The app passes everything a method needs as borrowed context; a plugin never sees `Hub`, `SessionRegistry`, `Dirs` or a session record, and runs no process.

```rust
// crates/agents/api/src/lib.rs
pub trait Agent: Send + Sync + 'static {
    type Options: DeserializeOwned + Serialize + Default;
    fn descriptor(&self) -> &AgentDescriptor;
    fn program_env_var(&self) -> &'static str;
    fn default_program(&self) -> &'static str;
    fn passthrough_vars(&self) -> &'static [&'static str] { &[] }
    fn liveness_tokens(&self) -> &'static [&'static str];
    fn check_request(&self, opts: &Self::Options, req: &SpawnCheck<'_>) -> Result<(), Refusal>;
    fn spawn_env(&self, opts: &Self::Options, rows: &ResolvedRows, keys: &dyn KeyLookup) -> Result<EnvPlan, Refusal>;
    fn conversation_plan(&self, opts: &Self::Options, resume: Option<&str>) -> ConversationPlan;
    fn hook_plan(&self, opts: &Self::Options, shim: &HookShim<'_>) -> HookPlan;
    fn interactive_args(&self, opts: &Self::Options, ctx: &SpawnContext<'_>, members: &[SessionMember], initial_prompt: Option<&str>) -> Vec<String>;
    fn headless_args(&self, opts: &Self::Options, ctx: &SpawnContext<'_>, prompt: &str) -> Option<Vec<String>>;
    fn map_hook_event(&self, body: &serde_json::Value, spawn: Option<&Self::Options>) -> Option<HookReport>;
    fn screen_rules(&self) -> &ScreenRules;
    fn ready_signal(&self) -> ReadySignal;
    fn file_reference(&self, repo_relative: &str) -> String;
    fn parse_headless_line(&self, line: &str) -> Vec<HeadlessEvent>;
    fn find_conversations(&self, query: &ConversationQuery<'_>) -> Vec<FoundConversation>;
    fn recovery_options(&self, run: &RecoveredRun<'_>) -> Self::Options;
    fn shell_resume(&self, opts: &Self::Options, id: &str, hook_files: &[PathBuf]) -> Result<Option<Vec<String>>, AgentError>;
    fn import_rev(&self) -> u32;
    fn import_tracer_run(&self, run: &TracerRun<'_>) -> Option<ImportedRun<Self::Options>>;
    #[cfg(feature = "test-support")]
    fn fake_cli(&self) -> Option<FakeCli>;
    fn user_settings_path(&self) -> Option<PathBuf>;
    fn install_user_hooks(&self, existing: Option<&str>, shim: &Path) -> Result<String, AgentError>;
    fn remove_user_hooks(&self, existing: Option<&str>, shim: &Path) -> Result<String, AgentError>;
    fn user_hooks_state(&self, existing: Option<&str>, shim: &Path) -> UserHooksState;
}
```

Methods with an obvious "does nothing" answer (`passthrough_vars`, `hook_plan`, `map_hook_event`, `parse_headless_line`, `find_conversations`, `shell_resume`, `import_tracer_run`, `fake_cli`, and the four user-hooks methods, which default to unsupported) have default bodies.

The optional `accounts(&AccountsConfig) -> Option<Arc<dyn AccountsService>>` (default `None`) gives the daemon an agent's saved-login service; the daemon's hub holds it, an elevated daemon never asks for it, and `conformance::check` also runs `check_accounts`.

The `accounts` module of `agent-api` holds the config, the `Send + Sync` service the hub shares with no lock, and its row types. The service has `snapshot`, `poll(PollScope::OnDemand)` (a `PollReport` of the new snapshot and `next_poll_in`) and `begin_shutdown(bound)` (stops new credential refreshes and waits for in-flight ones; true once idle).

The trait also carries `chat_entries` and `chat_line_facts` (the chat mirror) and `inherited_session_vars`. A failed decode of `OptionValues` into `Self::Options` is a conformance bug: on a spawn path the adapter refuses the spawn naming the agent, elsewhere it logs an `error!` and uses `Default`; the conformance test keeps it from happening.

| Capability | Method | What the app does with it |
|---|---|---|
| Identity and display | `descriptor` | Sends it to clients in `Agents`; validates options against its schema; logs with its id |
| Binary | `program_env_var`, `default_program` | Reads the override, then runs `resolve_agent_program`; `main.rs` logs every plugin's override |
| Environment | `passthrough_vars`, `spawn_env`, `inherited_session_vars` | The app's base keep-list plus the plugin's own variables, then the plugin's routing `set` and `remove`; `keys` is the app's `user_env` lookup, read at each spawn and never stored; the plugin lists the parent-session variables a child must not inherit (`tracer_client::inherited_session_vars()` unions them over the registry) |
| Request validation | `check_request` | Runs in `spawn_session` after the app's schema validation, before rows are sealed and before git work |
| Conversation id | `conversation_plan` | Acts on the plan (below); records the id as `conversation_id` and passes it in `SpawnContext` |
| Hooks | `hook_plan` | Writes `HookPlan::files` into `sessions/<id>/` (owning cleanup and discard), appends `HookPlan::args` with each `{file:name}` replaced by the written path; a file name must be one plain path component ending `.json`; a failed write runs without hooks |
| Spawn argv | `interactive_args`, `headless_args` | Builds the PTY or piped spawn; `None` from `headless_args` means "no headless" |
| Hook mapping | `map_hook_event` | The hook route buffers or applies the result generically (ordering, pending buffer, `apply_status`); the daemon keeps `apply_signal`, `apply_to_record` and `apply_details` over a `HookReport` |
| Screen status | `screen_rules` | Passed to `screen/rules.rs` (`CompiledRules::try_compile`), which every status watcher reads the screen with |
| Startup | `ready_signal` | `inject.rs` waits on it before an injector's first keystrokes (`StartupRule::agent_tui`; the floor and the cap stay daemon constants) |
| Prompt files | `file_reference` | The preset renderer formats a prompt file's path with it |
| Headless | `parse_headless_line` | The headless reader applies each `HeadlessEvent` |
| Conversations | `find_conversations` | Recover's candidates, the existence check before a recovery spawn, and the discovery poller for `DiscoverAfterLaunch` |
| Resume options | `recovery_options` | The options a recovered run spawns with |
| Shell resume | `shell_resume` | Called after the shell spawn wrote the plugin's hook files; returns the program and its arguments, which the daemon quotes for the shell's family and types, `Err` to refuse (a DeepSeek entry), or `Ok(None)` for an agent without one |
| Import | `import_rev`, `import_tracer_run` | The importer offers each tracer log to every plugin and stamps the plugin's revision; the first `Some` becomes the entry, else the app's shell rule applies |
| Chat | `chat_entries`, `chat_line_facts` | The chat mirror turns each transcript line into entries with `chat_entries`, and reads a line's `promptId`, `timestamp` and user-prompt flag (`TranscriptLineFacts`) with `chat_line_facts` (default: no facts) |
| Liveness | `liveness_tokens` | The orphan check's fallback for a sidecar with no `program_name` |
| Fake CLI | `fake_cli` | The live harness points each plugin's override at its fake |

The context and result types, all in `agent-api`:

- `SpawnContext`: `session_id`, `members: &[SessionMember]`, `add_dirs`, `cwd: &Path`, `initial_prompt`, `has_prompt_injector`, `resume`, `conversation_id`, `hook_args: &[String]` (from the written `HookPlan`).
- `ResolvedRows`: the spawn's env rows after the app's `${env:}` / `${secret:}` resolution, values included, never stored. Every query that reads the agent's own files carries them, so a plugin honours a session's own home variable. Claude's `claude_home` (`crates/agents/claude/src/transcripts.rs`) reads the session's `CLAUDE_CONFIG_DIR` row first, then the daemon's, then `~/.claude`.
- `HookShim`: the cached `rt-tracer.exe`, the session id and the config dir; a plugin builds its hook command from these. `HookPlan { files: Vec<HookFile { name, contents }>, args: Vec<String> }`: a settings file plus `--settings {file:hook-settings.json}` for Claude; argv-only tables for an agent like Codex, gated on its own options.
- `ConversationPlan`: `Minted(String)` (the plugin chose the id: Claude's UUID), `Resumed(String)`, `PreCreate { args: Vec<String>, parse: fn(&str) -> Option<String> }`, `DiscoverAfterLaunch`, `None`.
- `ConversationQuery`: the folder, a time window, a known id when one is recorded, the resolved rows. `FoundConversation { id, last_active, title }`.
- `HookReport`: `status`, `pending` (a `PendingUpdate`: `Clear`, `Set` or `KeepOr`), `conversation_id`, `last_message`, `failure` (the recent-actions line a failed turn leaves), `permission_mismatch` (to warn about), `agent_ended` (a `SessionEnd` that is not a `/clear`, which a plain shell hosting the agent acts on), `agent_started`, `subagent`, `background: Option<bool>` and `weak_idle`.

  `map_hook_event` takes the event body, the spawn's decoded options (`None` when the session has no record yet), and returns `None` for an event that says nothing.
- `HeadlessEvent`: `Status`, `RecentAction`, `Metrics { cost_usd, input_tokens, output_tokens }`, `ConversationId(String)`.
- `ReadySignal { min_output_bytes, quiet_for }` (the default is Claude's 1024 bytes and 1.5 s). `ScreenRules { rules }` is described under "Status rules"; `file_reference(repo_relative)` returns the prompt text naming a file (default the bare path, Claude's `@<path>`). `ImportedRun<O> { program_name, extra_dirs, options: O }`. `EnvPlan { set, remove }`. `Refusal { title, detail, hint }`.
- `hook_vocabulary`: a helper mapping Claude Code's hook event names and payload fields (`SessionStart`, `PreToolUse` with `AskUserQuestion` / `ExitPlanMode`, `Notification`, `Stop`, …) to a `HookReport`, so any agent whose hooks speak that vocabulary (Claude; Codex's `-c` hooks; the DeepSeek Harness bridge in [plans/agent-harnesses.md](./plans/agent-harnesses.md)) calls it instead of copying it.

`workspace_prelude` is an `agent-api` helper that takes the instruction-file names to mention, so the plugin passes "CLAUDE.md".

### What the app runs for a plugin

- **Pre-create.** For `PreCreate`, the app runs the resolved program with `args` in the session's cwd and environment, with its own timeout (`PRE_CREATE_TIMEOUT` in `crates/daemon/src/conversations/pre_create.rs`) and a process-tree kill on expiry, then feeds stdout to `parse`. A failure logs and spawns without a recorded conversation.
- **Discovery.** For `DiscoverAfterLaunch`, the app owns the poller, the claim arbitration across sessions waiting in one folder, a per-agent contested-ids store (`contested/<agent id>.json` in the config dir, entries dropping after 7 days) and a final look when the session ends, which runs inside `history::record_session_end`, inline, before the history entry is built. The plugin only answers `find_conversations`.
- **Early hook events.** The hook route buffers the raw body with its `X-RT-Hook-Time` while the session is not yet inserted, and maps it with the session's plugin inside `insert_with_pending`.

  The held events are raw JSON-object bodies, capped at 32 events and 256 KiB per session and 64 sessions and 8 MiB in all, oldest evicted first, dropped after 30 s; the record's plugin maps them, so the early `SessionStart` is mapped by the agent the record names and the permission-mismatch warn fires for early events too.
- **Shell resume.** A recovered shell that types an agent's resume command carries that agent as a daemon-internal `hook_agent` on its `SpawnArgs` and record, so the hook route maps its events with that plugin. It is never on the wire `SpawnRequest` and never stored in `SpawnConfig`; the recovery path sets it (`RecoveryPlan.hook_agent`, passed to `spawn_session_with_extras` as `SpawnExtras`), and the `meta.json` sidecar keeps it.

  A plain shell maps hooks only with a `hook_agent`. The shell itself stays a plain shell with no agent: its sidecar and history entry carry `agent: None`, and only the in-memory record uses `default_agent()`. Its snapshot carries `agent: None` too, until its screen watcher recognises an agent's TUI: the shell is then promoted, and the snapshot reports mode interactive, `agent` the plugin whose screen rules matched and `program_name` its tag (`session-runtime.md` "Session status"). A recovered shell with a `hook_agent` is never promoted by the screen.

### User hooks

`user_settings_path()` names the agent's own user-level settings file, and `install_user_hooks`, `remove_user_hooks` and `user_hooks_state` are pure text in, text out: the daemon reads and writes the file, the plugin only edits its text. `UserHooksState` is `Off`, `On`, `Partial` (some of the agent's entries are missing) or `Stale` (an entry points at another shim). A plugin that offers it sets `AgentCaps.user_hooks`; the defaults read as unsupported.

Claude's plugin edits Claude Code's user settings (`CLAUDE_CONFIG_DIR` when non-empty, else `~/.claude`, then `settings.json`). It appends an entry `{type: command, command: <shim>, args: ["hook", "--from-env"], async: true}` for the same events and matchers as the injected hooks (one event table builds both documents; the injected one is unchanged), repoints a stale copy (same args, a command whose file name is `rt-hook` or `rt-hook.exe`) and removes only its own entries.

It keeps the user's key order, indent, line endings and trailing newline, and refuses text that does not parse or a `hooks` section of another shape (`crates/agents/claude/src/hooks.rs`). The daemon side is in `docs/wire-protocol.md` (`GetAgentUserHooks`) and `docs/on-disk.md` ("User hooks").

### Codex and Cursor on this trait

Neither needs an app change to return:

| Need | Codex | Cursor |
|---|---|---|
| Binary | `RUSTLING_TULIP_CODEX` / `codex` | `RUSTLING_TULIP_CURSOR_AGENT` / `cursor-agent` |
| Options | `sandbox` (Choice), `hooks` (Toggle) | `plan_mode` (Toggle), `sandbox` (Choice) |
| Environment | `passthrough_vars`: `CODEX_HOME` and its key | its own key variables |
| Argv | `-C <cwd>` with the trust `-c`, `--add-dir`, `codex resume <id>` | `--workspace <cwd> --trust`, `--plan`, `--sandbox`, `--resume <id>`; `caps.extra_folders` false, so the app warns naming dropped folders |
| Hooks | `hook_plan` returns only `args` (`-c hooks.<Event>=…`, `--dangerously-bypass-hook-trust`) when `hooks` is on; `map_hook_event` calls `hook_vocabulary` | none |
| Conversation id | `DiscoverAfterLaunch`; `find_conversations` scans `$CODEX_HOME/sessions/` with the session's resolved rows; `Resumed` on resume | `PreCreate { args: ["create-chat"], parse }`, then `Resumed` |
| Headless | `parse_headless_line`, with `thread.started` as `ConversationId` | none |
| Screen rules | the `› 1.` selection line as a `PromptRule::Regex` | defaults |
| Shift+Enter | `Lf` | `Lf` |
| Recovery | `recovery_options` replays the stored options; `find_conversations` is the existence check | the recorded chat counts as existing |

## Wire protocol

Details of each message are in [wire-protocol.md](./wire-protocol.md); the shapes the seam relies on:

- **Naming an agent.** `AgentId(String)`, `#[serde(transparent)]`. A plain shell has no agent: `SessionSnapshot.agent`, `HistoryEntry.agent` and the sidecar's `agent` are `Option<AgentId>`, `None` for a shell.
- **Options.** `AgentSelection { agent: AgentId, options: OptionValues }`, where `OptionValues` is a `BTreeMap<String, OptionValue>` and `OptionValue` is untagged `Bool | Text | Null`. `SpawnRequest`, `SpawnConfig` and `PresetEntry` carry `agent: Option<AgentSelection>`, `None` exactly for a plain shell. `dangerously_skip_permissions` is a deprecated wire flag, not an option: true exactly when the options hold the descriptor's `bypass` choice, and a received `true` folds into that choice (`crates/daemon/src/trusted_fold.rs`).
- **Conversation.** `SessionSnapshot` and `HistoryEntry` carry `conversation_id`.
- **Recovery kinds.** `RecoverAs` is `Agent`, `RegisterRepoThenAgent { path }`, `ShellResume` (offered when `caps.shell_resume`) and `Shell` (a plain shell in the folder, nothing typed), plus `Unknown`.
- **Advertising agents.** `DaemonMessage::Agents { agents: Vec<AgentDescriptor> }`, sent to each connection right after `Welcome` and before `Sessions`. Registration is static, so it is sent once per connection.
- **Agent settings.** `PersistedState.agent_settings: BTreeMap<AgentId, OptionValues>` holds each agent's Settings page values; `ClientMessage::SetAgentSettings { agent, options }` sets one agent's, validated like a spawn's options; `DaemonMessage::AgentSettings` carries the whole map, sent after `Agents` and broadcast on every change. The spawn dialog's defaults are these values.
- **Chips.** The daemon evaluates the descriptor's chip rules into `SessionSnapshot.agent_chips: Vec<SessionChip { text, tip }>` (`agent_api::session_chips`), so a client draws them without evaluating options.

**Validation** is one function in `agent-api` over the descriptor, called by the daemon on every spawn request and every `SetAgentSettings`, before any seal or git work: an unknown agent id refuses; an unknown key or a value of the wrong type or outside its choices refuses naming the field; a missing key takes the field's default; a value a `when` rule hides is dropped.

Every default is written into the stored selection, so Duplicate, Restart and Recover replay what the session ran with. The plugin's `check_request` runs after it.

**The schema** (`protocol`; every growing enum carries `#[serde(other)] Unknown`, so a client older than its daemon still decodes a new field kind):

```rust
pub struct AgentDescriptor {
    pub id: AgentId,
    pub label: String,              // "Claude"
    pub tag: String,                // "claude", the runtime chip and selector part
    pub accent: AccentToken,        // a palette name; Unknown draws the default
    pub shift_enter: ShiftEnter,    // BackslashCr | Lf | Unknown
    pub caps: AgentCaps,            // headless, resume, extra_folders, shell_resume, user_hooks
    pub texts: AgentTexts,          // shell_resume_label, no_conversation
    pub fields: Vec<OptionField>,
    pub chips: Vec<ChipRule>,
    pub title: TitleRules,          // title glyphs and placeholder
    pub icon: Option<String>,       // runtime icon, SVG markup in its own colours
    pub bypass: Option<BypassChoice>, // { field, value }: the choice that bypasses permission prompts
}
pub struct OptionField {
    pub id: String,
    pub label: String,
    pub help: Option<String>,
    pub kind: FieldKind,
    pub default: OptionValue,
    pub settings: bool,             // on the agent's Settings page; its value is the dialog's default
    pub spawn: SpawnPlacement,      // Main | Advanced | Hidden | Unknown
    pub locked_when_trusted: bool,  // deprecated, always false
    pub when: Vec<FieldRule>,
}
pub enum FieldKind {
    Toggle,
    Choice { choices: Vec<Choice>, style: ChoiceStyle },
    Text { placeholder: Option<String> },
    Model { placeholder: Option<String>, suggestions: Vec<String> },
    #[serde(other)] Unknown,
}
pub struct FieldRule { pub if_field: String, pub equals: OptionValue, pub then: FieldOverride }
pub struct FieldOverride { pub hidden: Option<bool>, pub placeholder: Option<String>, pub suggestions: Option<Vec<String>> }
pub struct ChipRule { pub field: String, pub hide_when: OptionValue, pub tip: String }
```

Claude's descriptor holds `permission_mode` (Choice: CLI default, default, accept edits, bypass permissions, plan; Settings and spawn Advanced; its bypass permissions choice is the descriptor's `bypass` and passes `--permission-mode bypassPermissions`), `provider` (Choice Anthropic / DeepSeek, segmented; Settings and spawn Main), `model` (Model with the opus / sonnet / haiku suggestions, spawn Advanced only and not a Settings field, and a rule: when `provider` is `deepseek`, placeholder `deepseek-flash` and no suggestions), and one chip rule on `provider` hiding at `anthropic`. Its `texts` carry "Shell running claude --resume" and "no conversation found".

## Client rendering

A plain-Rust model `apps/native/src/agent_options.rs` holds one agent's descriptor and an `OptionValues`: defaults, the `when` rules, validation for the Text and Model kinds, and conversion to and from the wire map.

Its view `apps/native/src/agent_options_view.rs` draws a field list for a placement: Toggle as a switch, Choice as a segmented row or a dropdown by `style`, Text and Model as a text field (Model with its suggestion chips), and an Unknown kind as a disabled row reading "Update the client to change this". Keyboard stops follow the field order through the existing `spawn_form.rs` `move_focus` and `settings_view.rs` `SettingsControl` rings.

Where it mounts:

- Spawn dialog: `Runtime::Agent(AgentId)` lists the descriptors in daemon order, then Plain shell; the Main and Advanced sections render the agent's fields, starting from the agent's settings; Run mode offers Headless only when `caps.headless`; the request's deprecated `dangerously_skip_permissions` is derived from the descriptor's `bypass` choice. Duplicate and launch-last prefill an `AgentSelection` as stored.
- Settings: one page per descriptor under the non-selectable `Agents` group row, holding the fields with `settings: true`, read from `AgentSettings` and written with `SetAgentSettings`.
- Chips and labels: pane headers and leaves draw `agent_chips`; the runtime chip's label is `program_name`, else `tag`, else the agent id, and it is drawn as the descriptor's `icon` only while that label is the tag itself (a shell's label draws its shell's icon); Shift+Enter sends the bytes `shift_enter` names. A session with no name of its own shows its agent's tag.
- Recover dialog: offers the kinds `caps` allows, named by `label` and `texts`; its options and labels come from the descriptor and the daemon's `SessionHistoryItem.shell_resume`.
- The launch summary is built from the descriptor (`summary_fields`).

## Status rules

`pty_state.rs` keeps the hysteresis, the confirmation windows, the plain-shell classifier and the hook-driven demotions; what a screen shows is read by rules the session's plugin hands over. The rule table is data and the evaluator is the daemon's, because `regex` and the terminal grid (`Screen`) are daemon dependencies, never a plugin's.

`ScreenRules { rules: Vec<ScreenRule> }` is defined in `crates/agents/api/src/screen_rules.rs` and derives `Serialize` and `Deserialize`. A `ScreenRule` is `{ name, priority, region, matcher, not, verdict }`:

- `region` is a `ScreenRegion`: the last N non-empty rows (`BottomNonEmpty`), the rows between the prompt box's two rules (`PromptBoxBody`), the last N non-empty rows above the prompt box (`AbovePromptBox`), the last N non-empty rows counted from the last row whose text starts with a marker character, or with an optional second one, that row included, or the whole bottom when none does (`FromLastMarkedRow`, JSON `{"kind":"from_last_marked_row","marker":"❯","or_marker":"!","rows":24}`, `or_marker` optional), every row below the last horizontal rule row, the footer under the prompt box (`AfterLastRule`), or the OSC title (`Title`).

  Claude's dialog rules read `FromLastMarkedRow` anchored at the input box's `❯` cursor row, or its `!` row in bash mode (all but the question review step's, which has no footer and whose matched row can sit above its cursor), so a dialog footer quoted in a reply above an idle box never reads as a dialog. A `Title` rule with verdict `Working` also withholds a no-`Stop` `Idle` from a hook-driven turn while the terminal keeps writing (up to the quiet limit, `TITLE_QUIET_LIMIT`), so it matters beyond the ceiling; rows are physical, so a wrapped spinner row can miss its rule.
- `matcher` is a `LineMatcher`: `Contains` (a literal substring of a row) or `Regex` (matched per row). `not` is a list of matchers that veto the rule when any hits the same region.
- `verdict` is a `ScreenVerdict`: `Working`, `AwaitingInput`, `Idle`, or `Keep` (an overlay is open, so the status stays as it is).
- Every enum carries an `Unknown` variant that absorbs a value a newer plugin sends. A table with any bad rule (a structural problem, an unknown region, matcher, guard or verdict, an empty pattern, too many rules, an over-long pattern, too many guards, a regex that does not compile) is refused whole by `ScreenRules::validate`: the daemon logs one error and that agent's screen sets no status.

`screen/rules.rs` compiles a table once (`CompiledRules::try_compile`, rules sorted by descending priority) and reads a `Screen` with it (`CompiledRules::read`): the first rule whose matcher hits its region and none of whose `not` guards does decides the reading, else there is no match. `rules_for(agent)` caches one compiled set per agent. Every status watcher, a reattach and `agent_up` use the rules of the record's own agent; a plain shell uses `default_agent()`'s.

Claude's table is a constant in `agent-claude` (`crates/agents/claude/src/rules.rs`) beside its `ReadySignal` and `file_reference`; the conformance test checks every plugin's table. The screen fixture tests stay in the daemon, since they need `Screen`; `-p agent-claude rules` holds the data-shape tests. The timing constants (`IDLE_AFTER`, the echo budgets, the confirmation windows) stay app-wide.

Because the rules are a serde type, a plugin's rules can later load from a versioned data file with a user override without touching the trait; the built-in constant becomes the fallback.

## Testing

**Per-plugin tests** live in the agent crate and need no daemon: argv for each spawn shape, the environment plan, hook plans and the mapping of recorded hook payloads, the screen rules over recorded screens, the stream parser over recorded lines, transcript reading over fixture folders, recovery options and the tracer-log import. They run with `-p agent-claude`, mapped by a `crates/agents/claude/**` row in `tools/test-map.json`.

**The conformance test.** `agent-api`'s `test-support` feature exposes `conformance::check::<A: Agent>(&A)`: every field's default, every choice of every Choice field, each `when` rule's override and the empty map are validated by the app's schema function and then decoded into `A::Options`, and the struct's own serialization re-validates. Each agent crate calls it in one test, so a schema and its struct cannot drift.

**App tests** use `agent-api`'s `TestAgent` (an id, a schema, scripted argv, hook mapping and conversation plans), so daemon and client tests prove the generic paths, the pre-create runner and the discovery poller included, without naming Claude. Claude-specific daemon assertions (DeepSeek routing, `--settings`, `--resume`) live in `agent-claude`.

**The fake CLI.** Each plugin ships its own fake beside it (`crates/agents/claude/fake/`, `index.mjs` with `.cmd` and `.sh` shims), returned by `fake_cli` under `test-support`. The shim path `FakeCli` carries is relative to the workspace root, and the live harness joins it onto the checkout it was launched with, so an installed `rt-driver` runs the fake from the checkout its `launch` names rather than from the tree it was built in. The live harness iterates `agent-registry` and sets each plugin's `program_env_var` to its fake, so a later plugin brings its own fake without touching the harness. The live specs name "the first registered agent", never Claude.

**The seam check.** `tools/check-agent-names.ps1`: `rg -i` for `claude|anthropic|deepseek` over `crates/`, `apps/` and `tools/`, excluding the agent crates under `crates/agents/` other than `api` and `registry`.

Every remaining hit must match `tools/agent-names-allow.txt`, which lists paths or `path:pattern` pairs with a reason each. It runs as a prek hook and from `rt.ps1 clippy`, and fails naming each file and line. A prek hook runs on every commit, which matters in `test mode: unstable`, where a landing runs only its reached tests. The UI fixtures keep `"agent": "claude"` as test code, which the check does not scan.

## Prior art

- **herdr** (Apache-2.0; [research/herdr-comparison.md](./research/herdr-comparison.md)): per-agent screen-detection manifests shape `ScreenRules` as data rather than code; `pane.report_agent` as the agent-neutral report contract matches keeping `/hook/{session_id}` and `apply_to_record` in the app while each plugin owns only its vocabulary. Herdr plugins are argv commands with terminal-pane UI only, which is why this design sends a typed schema instead.
- **ThinkTerm** ([plans/borrowed-ideas.md](./plans/borrowed-ideas.md)): prompt rules in versioned per-agent data files with a user override, the model for the deserializable `ScreenRules`.
- **VS Code contribution points**: an extension declares its settings as a JSON schema in its manifest and the editor renders the Settings UI from it, with no per-extension UI code; the descriptor's `fields` follow that split between a declared schema and a host-drawn page.
