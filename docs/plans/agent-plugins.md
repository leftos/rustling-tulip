# Agent plugins: one module per agent behind one seam

Design for RT-265 (Design an agent plugin seam so each agent's code lives in one module); its build is RT-267 (Move all Claude support behind the agent seam as its first plugin), steps `AP.n` below. It starts from the code as it will stand after RT-266 (Delete Codex and Cursor agent support for the first release), which leaves Claude and the plain shell. The site inventory behind it is `.tmp/map-agents.md` (untracked); its findings are restated here where a step depends on them.

## Rulings this design builds on

Owner rulings (binding):

- Codex and Cursor are deleted first (RT-266, branch `feat/delete-codex-cursor`) and come back after the first release as further plugins on this seam.
- All existing Claude support becomes the first plugin: nothing outside the Claude module branches on Claude (RT-267). A later first release is accepted for this.
- An agent's UI comes from the daemon as a schema: each agent module declares its options (toggles, choices, text, model lists) as typed descriptors, the daemon sends them to clients, and the client renders Settings pages, spawn-dialog options and chips generically, with no per-agent client code. Remote and mobile clients get a new agent without a client release.
- Plugins are compiled in, one crate each (`crates/agents/claude`, later Codex and Cursor), each implementing one trait; the daemon registers them in a list. No runtime loader.
- **No backward compatibility until the first release.** The app is unreleased and is built as if from scratch: a wire or file shape change is simply made, with one protocol bump, and a `state.json`, preset, sidecar or history file that no longer decodes is ignored (a fresh start is fine). Sessions and history of a removed agent are not handled. This replaces the earlier rulings that dropped a removed agent's saved choices on load and listed its history as "not available".
- **An agent's Settings values live on the daemon** (AQ2 (a)): in `state.json`, per agent, pushed to every client; the client's old Claude defaults in `native-ui.json` are no longer read.
- **DeepSeek routing is a Claude plugin option** (AQ6 (a)): a Provider choice in Claude's schema; only the Claude crate knows the environment mapping, and the app only looks keys up.

Orchestrator decisions, made while scoping RT-265:

- The plain shell is the app's built-in baseline, not a plugin: `SessionMode::PlainShell`, `spawn_plain_shell_session` (`crates/daemon/src/server/spawn.rs`), `resolve_shell_program`, `pty_state::watch_plain_shell` and the client's `Runtime::PlainShell` (`apps/native/src/spawn_form.rs`) stay app code.
- The shell's Claude leaks are removed by this work: the shell record stamped `agent: Agent::Claude` (`server.rs:4849`), its sidecar written with `Agent::Claude` (`server.rs:4924`), the client sending `AgentOptions::claude(None)` for a shell (`apps/native/src/shell_dialog.rs:241`), and a recovered shell carrying Claude hooks and typing `claude --resume` (`server.rs:4766-4771`, `crates/daemon/src/history.rs:1404-1431`).

Orchestrator decisions, from an independent review of the first draft against the code:

- **The trait is complete enough for Codex and Cursor to return with no app change** (walked through below): argv-injected hooks, pre-created and discovered conversation ids, per-agent passthrough variables, regex screen rules, a startup ready signal and a prompt file-reference syntax are plugin capabilities, and the app runs every process, poller and timeout.
- **Typed options inside each plugin.** A value map travels on the wire and on disk, validated by the app's one schema function; each plugin works with its own `Options` struct, adapted by `agent-api`, and a conformance test runs every descriptor through its struct.
- **AQ1**: the API crate is `crates/agents/api`, package `agent-api`.
- **AQ3**: dependent fields are declarative `when` rules on a field, evaluated by the client and applied by the daemon too (a value a rule hides is dropped daemon-side).
- **AQ4**: `Welcome.features` stays for app-level capabilities; `claude_provider` leaves it.
- **AQ5**: the trait is synchronous; the app runs every process and wait itself.
- **AQ7**: `model` is a field of the agent's schema.

## Goal and scope

Every agent-specific decision lives in that agent's crate. The app (daemon, protocol, native client) asks a registered plugin for what it needs through one trait and one wire schema, and never names an agent. Claude is moved behind the seam with its behaviour unchanged; the seam is proven by an automated check that no app file names Claude (see Testing).

Inside the seam (the plugin owns it): the CLI's binary name and override variable; its argv for interactive and headless runs; its environment routing (DeepSeek) and the variables it needs passed through; its hook files, hook arguments and hook vocabulary; its screen prompt rules; how its conversation id is chosen or found, its transcripts and resume flags.

The plugin also owns its recovery choices and shell-typed resume command; its headless stream parser; its option schema and chips; its startup ready signal and prompt file-reference syntax; its tracer-log import; its fake CLI for tests.

Outside the seam (app code, agent-neutral): the plain shell; the PTY, `rt-tracer.exe`, the tracer ABI and reattach; scrollback; tabs and layouts; worktrees, workspaces and git. The hook side the app keeps is the `/hook/{session_id}` route, the hook ordering stamp, the early-event buffer and `apply_status` (`crates/daemon/src/hook_status.rs`, `pty_state.rs`), plus the idle demotions and the byte-volume and title heuristics.

Also app code: env-row sealing and `${env:}` / `${secret:}` resolution; the base environment keep-list; npm-shim resolution (`resolve_agent_program`, `server/spawn/shell.rs`); the trusted-launch flag (`dangerously_skip_permissions`) and its `elevated_authority` chip; every process the app runs for a plugin (a pre-create, a discovery poll) with its timeout and kill; history storage, pruning and the recover dialog's layout; the schema validation function.

## Crate layout

```
crates/agents/api/       package `agent-api`: the typed `Agent` trait, the object-safe `AgentPlugin`, context types, the schema validator, the shared hook-vocabulary helper
crates/agents/claude/    package `agent-claude`: the Claude plugin (argv, DeepSeek routing, hooks, rules, transcripts, schema, fake CLI)
crates/agents/registry/  package `agent-registry`: the one list of registered plugins and lookup by id
crates/protocol/         the wire half of the schema (AgentId, AgentSelection, AgentDescriptor, OptionField, OptionValues)
```

Dependency rules:

- `protocol` depends on no agent crate. The schema types are wire types, so they live in `protocol` (CLAUDE.md, "Single source of truth for the wire protocol").
- `agent-api` depends on `protocol`, `serde`, `serde_json`, `regex` and `anyhow` only: never on `daemon`, `tracer`, `daemon-client` or `apps/native`.
- An agent crate depends on `agent-api` (which re-exports the protocol types it uses) and its own parsing needs; never on the daemon, the client or another agent crate.
- `agent-registry` depends on `agent-api` and every agent crate, and holds `REGISTERED: &[&dyn AgentPlugin]`, `plugin(&AgentId)`, `all()`, `descriptors()` and `default_agent()` (the first registered). It is its own crate because the daemon is bin-only (`crates/daemon/Cargo.toml:12-14`) and the native client's live tests need the same list to point each plugin's override at its fake CLI (a dev-dependency of `apps/native`, never a normal one).
- `daemon` depends on `agent-api` and `agent-registry` only. `apps/native` depends on `protocol` only in its normal build: everything it knows about an agent arrives in the descriptor list.
- `agent-registry`'s `lib.rs` and `Cargo.toml` are the only files outside an agent crate that name one; the seam check allows exactly those (and `Cargo.lock`).

`backend_for` (`crates/daemon/src/agents/mod.rs:254`) and `Agent::all` (`crates/protocol/src/lib.rs:48`) are replaced by the registry. A request naming an id the registry lacks is refused ("This daemon has no agent `x`.").

## The trait

A plugin implements the typed trait; `agent-api` adapts every implementor to the object-safe `AgentPlugin` the registry holds, decoding the validated `OptionValues` into `Self::Options` once per call. Plugins are stateless statics, as `AgentBackend` implementors are today (`agents/mod.rs:247`). The app passes everything a method needs as borrowed context; a plugin never sees `Hub`, `SessionRegistry`, `Dirs` or a session record, and runs no process.

```rust
// crates/agents/api/src/lib.rs
pub trait Agent: Send + Sync + 'static {
    type Options: DeserializeOwned + Serialize + Default;
    fn descriptor(&self) -> &AgentDescriptor;
    fn program_env_var(&self) -> &'static str;
    fn default_program(&self) -> &'static str;
    fn passthrough_vars(&self) -> &'static [&'static str];
    fn liveness_tokens(&self) -> &'static [&'static str];
    fn check_request(&self, opts: &Self::Options, req: &SpawnCheck<'_>) -> Result<(), Refusal>;
    fn spawn_env(&self, opts: &Self::Options, rows: &ResolvedRows, keys: &dyn KeyLookup) -> Result<EnvPlan, Refusal>;
    fn conversation_plan(&self, opts: &Self::Options, resume: Option<&str>) -> ConversationPlan;
    fn hook_plan(&self, opts: &Self::Options, trusted: bool, shim: &HookShim<'_>) -> HookPlan;
    fn interactive_args(&self, opts: &Self::Options, ctx: &SpawnContext<'_>) -> Vec<String>;
    fn headless_args(&self, opts: &Self::Options, ctx: &SpawnContext<'_>, prompt: &str) -> Option<Vec<String>>;
    fn map_hook_event(&self, body: &serde_json::Value, spawn: Option<&Self::Options>) -> Option<HookReport>;
    fn screen_rules(&self) -> &ScreenRules;
    fn ready_signal(&self) -> ReadySignal;
    fn file_reference(&self, repo_relative: &str) -> String;
    fn parse_headless_line(&self, line: &str) -> Vec<HeadlessEvent>;
    fn find_conversations(&self, query: &ConversationQuery<'_>) -> Vec<FoundConversation>;
    fn recovery_options(&self, run: &RecoveredRun<'_>) -> Self::Options;
    fn shell_resume(&self, opts: &Self::Options, id: &str, hook_files: &[PathBuf]) -> Result<Option<String>, String>;
    fn import_rev(&self) -> u32;
    fn import_tracer_run(&self, run: &TracerRun<'_>) -> Option<ImportedRun<Self::Options>>;
    #[cfg(feature = "test-support")]
    fn fake_cli(&self) -> Option<FakeCli>;
}
```

Methods with an obvious "does nothing" answer (`passthrough_vars`, `hook_plan`, `map_hook_event`, `parse_headless_line`, `find_conversations`, `shell_resume`, `import_tracer_run`, `fake_cli`) have default bodies. A failed decode of `OptionValues` into `Self::Options` is a conformance bug: on a spawn path the adapter refuses the spawn naming the agent, elsewhere it logs an `error!` and uses `Default`; the conformance test (Testing) keeps it from happening.

| Capability | Method | What the app does with it | Sites it replaces |
|---|---|---|---|
| Identity and display | `descriptor` | Sends it to clients in `Agents`; validates options against its schema; logs with its id | `Agent::as_label` (`protocol/src/lib.rs:36`), `agent_name` (`history.rs:1093`), the client's label, accent and chip tables (Client rendering) |
| Binary | `program_env_var`, `default_program` | Reads the override, then runs `resolve_agent_program`; `main.rs` logs every plugin's override | `AgentBackend::resolve_program` (`agents/mod.rs:221`), `claude.rs:18-24`, `main.rs:96-97` |
| Environment | `passthrough_vars`, `spawn_env` | The app's base keep-list (`PATH`, `HOME`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `TEMP`, `TMP`, `TERM`, `LANG`, `LC_ALL`) plus the plugin's own variables, then the plugin's routing `set` and `remove`; `keys` is the app's `user_env` lookup, read at each spawn and never stored | `passthrough_env` (`providers.rs:292-310`, whose `ANTHROPIC_API_KEY` and `CLAUDE_CONFIG_DIR` become Claude's), `providers::spawn_env` (`server.rs:3742`), the DeepSeek table (`providers.rs:43-82`) |
| Request validation | `check_request` | Runs in `spawn_session` after the app's schema validation, before rows are sealed and before git work | `providers::check_request` (`server.rs:3686-3687`), `request_provider` (`server.rs:7750-7770`) |
| Conversation id | `conversation_plan` | Acts on the plan (below); records the id as `conversation_id` and passes it in `SpawnContext` | `server.rs:4537-4541` |
| Hooks | `hook_plan` | Writes `HookPlan::files` into `sessions/<id>/` (owning cleanup and discard), appends `HookPlan::args` with each `{file:name}` replaced by the written path; a failed write runs without hooks | `write_hook_settings` (`server.rs:4391-4432`, its Claude gate at 4392), `build_hook_settings` (`claude.rs:220-273`) |
| Spawn argv | `interactive_args`, `headless_args` | Builds the PTY or piped spawn; `None` from `headless_args` means "no headless" | `build_interactive_args` / `build_headless_args` (`agents/mod.rs:186-205`), `server.rs:3728-3734`, `standalone_supports` (`server.rs:6042`) |
| Hook mapping | `map_hook_event` | The hook route buffers or applies the result generically (ordering, pending buffer, `apply_status`) | `hook_status::map_event`, `details`, `permission_mismatch`, `update_conversation_id` (`hook_status.rs:61-177`, 106-125, 419-425) |
| Screen status | `screen_rules` | Passed to `pty_state::watch` in place of `matches_prompt` | `pty_state.rs:540-556` |
| Startup | `ready_signal` | `inject.rs` waits on it before an injector's first keystrokes | the Claude-banner tuning in `inject.rs:4-23`, `44-58`, `86-89` |
| Prompt files | `file_reference` | The preset renderer formats a prompt file's path with it | `@<repo-relative-path>` in `presets.rs:459` |
| Headless | `parse_headless_line` | The headless reader applies each `HeadlessEvent` | `handle_headless_line` (`claude.rs:114-165`), `headless.rs:128-140` |
| Conversations | `find_conversations` | Recover's candidates, the existence check before a recovery spawn, and the discovery poller for `DiscoverAfterLaunch` | `transcripts.rs` (whole file), `history.rs:852-874`, `recovery_conversation_exists` (`server.rs:6465-6483`) |
| Resume options | `recovery_options` | The options a recovered run spawns with | `recovered_claude_flags` (`history.rs:1361-1402`) |
| Shell resume | `shell_resume` | Called after the shell spawn wrote the plugin's hook files; returns the final command to type, `Err` to refuse (a DeepSeek entry), or `Ok(None)` for an agent without one | `shell_request` (`history.rs:1404-1431`), `shell_allowed` (`history.rs:1043-1053`), `render_shell_resume` (`server.rs:4355-4365`) |
| Import | `import_rev`, `import_tracer_run` | The importer offers each tracer log to every plugin and stamps the plugin's revision; the first `Some` becomes the entry, else the app's shell rule applies | `entry_from_tracer_log` (`history.rs:616-676`), `imported_provider`, `model_arg`, the app-wide `IMPORT_REV` (`history.rs:56`) |
| Liveness | `liveness_tokens` | The orphan check's fallback for a sidecar with no `program_name` | `orphan.rs:417-423` |
| Fake CLI | `fake_cli` | The live harness points each plugin's override at its fake | `tools/e2e/fake-claude/`, `apps/native/tests/support/live.rs` |

The context and result types, all in `agent-api`:

- `SpawnContext`: `session_id`, `trusted`, `members: &[SessionMember]`, `add_dirs`, `cwd: &Path`, `initial_prompt`, `has_prompt_injector`, `resume`, `conversation_id`, `hook_args: &[String]` (from the written `HookPlan`). It replaces `CommonSpawnFields` (`agents/mod.rs:121-163`), whose `claude_session_id` and `hook_settings` name Claude.
- `ResolvedRows`: the spawn's env rows after the app's `${env:}` / `${secret:}` resolution, values included, never stored. Every query that reads the agent's own files carries them, so a plugin honours a session's own home variable. Today `transcripts::claude_home` (`transcripts.rs:37-39`) reads `CLAUDE_CONFIG_DIR` from the daemon's environment only and misses a session's row, an existing limitation this fixes.
- `HookShim`: the cached `rt-tracer.exe`, the session id and the config dir; a plugin builds its hook command from these. `HookPlan { files: Vec<HookFile { name, contents }>, args: Vec<String> }`: a settings file plus `--settings {file:hook-settings.json}` for Claude; argv-only tables for an agent like Codex, gated on its own options.
- `ConversationPlan`: `Minted(String)` (the plugin chose the id: Claude's UUID), `Resumed(String)`, `PreCreate { args: Vec<String>, parse: fn(&str) -> Option<String> }`, `DiscoverAfterLaunch`, `None`.
- `ConversationQuery`: the folder, a time window, a known id when one is recorded, the resolved rows. `FoundConversation { id, last_active, title }`.
- `HookReport`: status, pending-input update, a new conversation id, the turn's last message, a failure line, a permission mismatch to warn about, and `agent_ended` (today's `SessionEnd` special case for a plain shell, `hook_status.rs:252-258`).
- `HeadlessEvent`: `Status`, `RecentAction`, `Metrics { cost_usd, input_tokens, output_tokens }`, `ConversationId(String)`.
- `ReadySignal { min_output_bytes, quiet_for }` (Claude's 1024 bytes and 1.5 s). `ImportedRun<O> { program_name, extra_dirs, options: O, trusted }`. `EnvPlan { set, remove }` (today's `SpawnEnv`). `Refusal { title, detail, hint }` (today's `SpawnFailure`).
- `hook_vocabulary`: a helper mapping Claude Code's hook event names and payload fields (`SessionStart`, `PreToolUse` with `AskUserQuestion` / `ExitPlanMode`, `Notification`, `Stop`, …) to a `HookReport`, so any agent whose hooks speak that vocabulary (Claude; Codex's `-c` hooks; the DeepSeek Harness bridge in `agent-harnesses.md`) calls it instead of copying it.

`workspace_prelude` (`agents/mod.rs:271-295`) moves into `agent-api` as a helper that takes the instruction-file names to mention, so "CLAUDE.md" is passed by the plugin.

### What the app runs for a plugin

- **Pre-create.** For `PreCreate`, the app runs the resolved program with `args` in the session's cwd and environment, with its own timeout (today's 5 s, `agents/mod.rs:27`) and a process-tree kill on expiry, then feeds stdout to `parse`. A failure logs and spawns without a recorded conversation.
- **Discovery.** For `DiscoverAfterLaunch`, the app owns the poller, the claim arbitration across sessions waiting in one folder, a per-agent contested-ids store (`contested/<agent id>.json` in the config dir, entries dropping after 7 days) and a final look when the session ends. The plugin only answers `find_conversations`.
- **Early hook events.** The hook route buffers the raw body with its `X-RT-Hook-Time` while the session is not yet inserted, and maps it with the session's plugin inside `insert_with_pending`. Today it maps before buffering (`server.rs:694-699`, `hook_status.rs:286-343`); with mapping per agent, the early `SessionStart` arrives before the record that says which agent it is.
- **Shell resume.** A recovered shell that types an agent's resume command carries that agent as a daemon-internal `hook_agent` on its `SpawnArgs` and record, so the hook route maps its events with that plugin. It is never on the wire `SpawnRequest` and never stored in `SpawnConfig`; the recovery path sets it (`history.rs:1431`, `server.rs:4766-4771`). The shell itself stays a plain shell with no agent.
- **Default agent.** A preset that names no agent runs the registry's `default_agent()`, replacing `#[default] Claude` (`protocol/src/lib.rs:27`).

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

One breaking change, one bump: `protocol-version.json` from RT-266's 24 to `{"version": 25, "supported": [25]}` in AP.6, after confirming that `daemon_client::ensure_running` retires a running v24 daemon over `/shutdown` (`crates/daemon-client/src/supervisor.rs`, `native_client_retires_daemon_that_speaks_only_an_older_version`).

Stored files written in the old shapes that no longer decode are ignored, per the owner's ruling. The `feat/remote-mode` branch is at 23 today; if it lands a breaking change first, the two share or sequence their bumps.

- **Naming an agent.** `Agent` becomes `AgentId(String)`, `#[serde(transparent)]`. A plain shell has no agent: `SessionSnapshot.agent`, `HistoryEntry.agent` and the sidecar's `agent` are `Option<AgentId>`, `None` for a shell.
- **Options.** `AgentOptions` is replaced by `AgentSelection { agent: AgentId, options: OptionValues }`, where `OptionValues` is a `BTreeMap<String, OptionValue>` and `OptionValue` is untagged `Bool | Text | Null`. `SpawnRequest`, `SpawnConfig` and `PresetEntry` carry `agent: Option<AgentSelection>`, `None` exactly for a plain shell.

  `SpawnRequest.model` goes (it is the `model` field); `dangerously_skip_permissions` stays as the app's trusted flag. The v18 legacy deserializers of `SpawnConfig` and `PresetEntry` (`protocol/src/lib.rs:1002-1052`, 1853-1975) are deleted.
- **Gone.** `ClaudeProvider`, `PermissionMode`, `FEATURE_CLAUDE_PROVIDER` (`lib.rs:17`, listed at `server.rs:1781`), `SessionSnapshot.claude_provider`, `HistoryEntry.provider` / `model` / `skip_permissions` (an import's are in its options and `trusted`). `claude_session_id` becomes `conversation_id` on `SessionSnapshot` and `HistoryEntry`.
- **Recovery kinds.** `RecoverAs` is `Agent`, `RegisterRepoThenAgent { path }`, `ShellResume` (offered when `caps.shell_resume`) and `Shell` (a plain shell in the folder, nothing typed), plus `Unknown`.
- **Advertising agents.** `DaemonMessage::Agents { agents: Vec<AgentDescriptor> }`, sent to each connection right after `Welcome` and before `Sessions`. Registration is static, so it is sent once per connection.
- **Agent settings.** `PersistedState.agent_settings: BTreeMap<AgentId, OptionValues>` holds each agent's Settings page values; `ClientMessage::SetAgentSettings { agent, options }` sets one agent's, validated like a spawn's options; `DaemonMessage::AgentSettings` carries the whole map, sent after `Agents` and broadcast on every change. The spawn dialog's defaults are these values.
- **Chips.** The daemon evaluates the descriptor's chip rules into `SessionSnapshot.agent_chips: Vec<SessionChip { text, tip }>`, so a client draws them without evaluating options.

**Validation** is one function in `agent-api` over the descriptor, called by the daemon on every spawn request and every `SetAgentSettings`, before any seal or git work: an unknown agent id refuses; an unknown key or a value of the wrong type or outside its choices refuses naming the field; a missing key takes the field's default; a value a `when` rule hides is dropped.

Every default is written into the stored selection, so Duplicate, Restart and Recover replay what the session ran with. The plugin's `check_request` runs after it.

**The schema** (`protocol`; every growing enum carries `#[serde(other)] Unknown` per CLAUDE.md, so a client older than its daemon still decodes a new field kind):

```rust
pub struct AgentDescriptor {
    pub id: AgentId,
    pub label: String,              // "Claude"
    pub tag: String,                // "claude", the runtime chip and selector part
    pub accent: AccentToken,        // a palette name; Unknown draws the default
    pub shift_enter: ShiftEnter,    // BackslashCr | Lf | Unknown
    pub caps: AgentCaps,            // headless, resume, extra_folders, shell_resume
    pub trust_note: Option<String>, // what trusted launch passes, for the dialog's note
    pub texts: AgentTexts,          // shell_resume_label, no_conversation
    pub fields: Vec<OptionField>,
    pub chips: Vec<ChipRule>,
}
pub struct OptionField {
    pub id: String,
    pub label: String,
    pub help: Option<String>,
    pub kind: FieldKind,
    pub default: OptionValue,
    pub settings: bool,             // on the agent's Settings page; its value is the dialog's default
    pub spawn: SpawnPlacement,      // Main | Advanced | Hidden | Unknown
    pub locked_when_trusted: bool,
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

Claude's descriptor holds `permission_mode` (Choice: CLI default, default, accept edits, bypass permissions, plan; Settings and spawn Main; locked when trusted), `provider` (Choice Anthropic / DeepSeek, segmented; Settings and spawn Main), `model` (Model with the opus / sonnet / haiku suggestions and a rule: when `provider` is `deepseek`, placeholder `deepseek-flash` and no suggestions), and one chip rule on `provider` hiding at `anthropic`.

Its `texts` carry "Shell running claude --resume" and "no conversation found" (`apps/native/src/recover.rs:391`, `475`).

## Client rendering

A plain-Rust model `apps/native/src/agent_options.rs` holds one agent's descriptor and an `OptionValues`: defaults, the `when` rules, the trusted lock, validation for the Text and Model kinds, and conversion to and from the wire map.

Its view `apps/native/src/agent_options_view.rs` draws a field list for a placement: Toggle as a switch, Choice as a segmented row or a dropdown by `style`, Text and Model as a text field (Model with its suggestion chips), and an Unknown kind as a disabled row reading "Update the client to change this". Keyboard stops follow the field order through the existing `spawn_form.rs` `move_focus` and `settings_view.rs` `SettingsControl` rings.

Where it mounts:

- Spawn dialog: `Runtime::Agent(AgentId)` lists the descriptors in daemon order, then Plain shell; the Main and Advanced sections render the agent's fields, starting from the agent's settings; Run mode offers Headless only when `caps.headless`; the trusted note reads `trust_note`. Duplicate and launch-last prefill an `AgentSelection` as stored.
- Settings: one page per descriptor under the existing non-selectable `Agents` group row, holding the fields with `settings: true`, read from `AgentSettings` and written with `SetAgentSettings`.
- Chips and labels: pane headers and leaves draw `agent_chips`; the runtime chip text is `tag` and its colour `accent`; Shift+Enter sends the bytes `shift_enter` names.
- Recover dialog: offers the kinds `caps` allows, named by `label` and `texts`.

What it replaces (inventory group 8, Claude rows, after RT-266 has removed the Codex and Cursor ones):

- `spawn_form.rs`: `Runtime::ALL` (106-111); `MODEL_ALIASES`, `DEEPSEEK_MODEL_PLACEHOLDER`, `PROVIDER_CHOICES`, `PROVIDER_UNSUPPORTED_TIP`, `provider_label` and the chip helpers, `stored_claude_provider`, `APPROVAL_CHOICES`, `approval_label` (153-271); `model_chips_shown`, `provider_shown` and the `Agent::Claude` checks (2190-2211); the per-agent control ring (2720) and the `agent_options()` builder (3014-3040).
- `spawn_view.rs`: the Claude Advanced section and the provider row (2470-2530). `settings_view.rs`: `SettingsTab::Claude`, its page and its provider row (91, 1396-1426, 1789-1830).
- `sidebar.rs`: `SpawnDefaults.permission_mode` / `claude_provider` and `known_provider` (389-433), `agent_summary_label` / `authority_label` (1444-1470), `chip_provider` (1818-1827). `sidebar_view.rs`: `runtime_color` (1606-1615). `grid_view.rs`: the provider chip.
- `term_input.rs`: `shift_enter` (45-51). `recover.rs`: the Claude recovery options, labels and provider gate (350-418, 391, 475). `connection.rs`: the `FEATURE_CLAUDE_PROVIDER` checks. `shell_dialog.rs`: `AgentOptions::claude(None)` (241). `quit.rs`: "owns your claude sessions" (18).

## Status rules

`pty_state.rs` keeps the classifier, the volume window, the title heartbeat, `IDLE_AFTER` and the hook-driven demotions. `watch` takes `&ScreenRules` from the session's plugin, and `prompt_on_screen` evaluates those rules over the stripped 2 KiB tail instead of calling `matches_prompt`.

`ScreenRules` is data, defined in `agent-api` and deriving `Deserialize` from the start:

```rust
pub struct ScreenRules { pub prompts: Vec<PromptRule> }
pub enum PromptRule {
    Contains(String),
    AllOf(Vec<String>),
    NumberedChoiceWithCursor { cursors: Vec<String> },
    Regex(String),            // compiled once per rule set; `regex` is already a daemon dependency
    #[serde(other)] Unknown,
}
```

Claude's rules are a constant in `agent-claude` that reproduces `matches_prompt` exactly: `Do you want to`, `Choose an option`, `Press 1`, `(y/n)`, all of `│` and `AskUserQuestion`, and a `1.` / `2.` pair with a `❯` or `>>>` cursor. The timing constants (`IDLE_AFTER`, the echo budgets) stay app-wide; moving them into the rules waits for an agent that needs different ones.

Path to RT-74 (Steadier screen-based status: hysteresis, rule files, agent process detection): because the rules are already a serde type, RT-74 can load a plugin's rules from a versioned data file with a user override without touching the trait; the built-in constant becomes the fallback. RT-74's design is not made here. RT-264 edits `matches_prompt` directly; it lands before AP.4, which carries its result over as data.

## Claude as the first plugin (RT-267)

Each step lands on `main` as one brief of about 100 implementer tool calls and one commit that builds and passes its reached tests, except AP.6a and AP.6b, which land as one commit because the protocol change breaks the daemon until the daemon follows it. Behaviour stays the same through AP.5; AP.6 changes the wire; AP.7 and AP.8 make the client generic; AP.9 moves the fake CLI and turns the seam check on.

Every proof runs as `pwsh -NoProfile -File tools/gate.ps1 run … -- cargo nextest run … --cargo-profile test-release`, with the named filter red first where a step adds behaviour.

- [x] **AP.1a Stand up the crates and the registry, Claude moved in unchanged.** New `crates/agents/api` (package `agent-api`) with `AgentPlugin` and the context types; new `crates/agents/claude` (package `agent-claude`) holding `agents/claude.rs` and its tests; new `crates/agents/registry` (package `agent-registry`); the daemon looks plugins up through the registry instead of `backend_for`; `headless.rs` applies `HeadlessEvent`s instead of passing the registry into the parser.

  Files: workspace `Cargo.toml`, the three new crates, `crates/daemon/Cargo.toml`, `agents/mod.rs` (deleted), `agents/claude.rs` (moved), `headless.rs`, `server/spawn.rs` (the former `backend_for` sites), `providers.rs` (moved from `agents/providers.rs`), `tools/test-map.json`. Proof: `-p agent-claude` (the moved argv and stream-json tests), `-p daemon headless`, `-p daemon spawn`.

  Landed on main.
- [x] **AP.1b The trait's final shape and typed options.** The typed `Agent` trait with `type Options`, the adapter to `AgentPlugin`, `OptionValues` in every signature, `ClaudeOptions { permission_mode, provider, model }`; until AP.6, `spawn_session` converts the request's `AgentOptions` and `model` into `OptionValues` in one place; Claude's descriptor fields; the conformance test.

  Files: `agent-api` (trait, adapter, `conformance` under `test-support`), `agent-claude` (options, descriptor), `server/spawn.rs` (the converter, `SpawnArgs`). Proof: `-p agent-api` (adapter decode, a failed decode refuses), `-p agent-claude conformance`, `-p daemon spawn`.
- [ ] **AP.2a Environment behind the seam.** The base keep-list moves to app code and each plugin adds `passthrough_vars`; `providers.rs` moves into `agent-claude` behind `check_request` and `spawn_env`; `user_env.rs` loses its DeepSeek names behind `KeyLookup`; `ResolvedRows` carries the resolved rows.

  Files: `providers.rs` (moved), `user_env.rs`, `server.rs` (`spawn_session` pre-checks, `request_provider`, the three spawn fns' env), `agent-claude` env module. Proof: `-p agent-claude providers`, the server DeepSeek tests (`-p daemon deepseek`), a headless child gets `ANTHROPIC_API_KEY` only through Claude's list.
- [ ] **AP.2b Conversation plans and conversations.** `conversation_plan` replaces `server.rs:4537-4541`; the app's `PreCreate` runner and `DiscoverAfterLaunch` poller, with claim arbitration and the contested store, proven with `TestAgent`; `transcripts.rs` moves into `agent-claude` behind `find_conversations`, taking the session's resolved rows; `main.rs` logs every plugin's override.

  Files: `transcripts.rs` (moved), a new `crates/daemon/src/conversations.rs`, `server.rs` (`session_history_items`, `recovery_conversation_exists`), `history.rs` (`conversation_candidates`), `main.rs`. Proof: `-p agent-claude transcripts` (a session's `CLAUDE_CONFIG_DIR` row is honoured), `-p daemon conversations` (a pre-create timeout kills the tree; two sessions in one folder never both claim an id), `-p daemon history`.
- [ ] **AP.3a Hook plans and mapping.** `hook_plan` replaces `write_hook_settings`'s Claude gate and `build_hook_settings`; `hook_vocabulary` in `agent-api` and Claude's `map_hook_event` take over `map_event`, `details`, `permission_mismatch` and the conversation-id update, leaving `hook_status.rs` with ordering, the pending buffer and `apply_to_record` over a `HookReport`.

  Files: `hook_status.rs`, `server.rs` (`write_hook_settings`, `hook_handler`, `HookSettings`), `agent-api` `hook_vocabulary`, `agent-claude` hooks module. Proof: `-p agent-api hook_vocabulary`, `-p agent-claude hooks`, `-p daemon hook_status`, `-p daemon hook`.
- [ ] **AP.3b Early events, the shell's hook agent and its leaks.** The hook route buffers raw bodies with their time and maps them in `insert_with_pending`; the daemon-internal `hook_agent`; `shell_resume` runs after the hook files are written and returns the final command, so the `{rt-hook-settings}` slot and its strip at `server.rs:4362` go; the shell record and sidecar stop carrying `Agent::Claude` internally (`server.rs:4849`, `4924`).

  Files: `hook_status.rs` (`PendingHooks`), `server.rs` (`hook_handler`, `spawn_plain_shell_session`, `render_shell_resume`), `session.rs`, `orphan.rs`, `history.rs` (`shell_request`), `agent-claude` recovery module.

  Proof: an early `SessionStart` held, then applied with the right plugin (`-p daemon hook_status`); the recovered-shell command test (`-p daemon shell_resume`); `.\rt.ps1 native-e2e` for `live_hook_status_reports_working_asking_and_idle` and the recovered-shell path in `e2e_recover.rs`.
- [ ] **AP.4 Screen rules, startup and file references.** `ScreenRules` with `Regex`; `pty_state::watch` takes the plugin's rules; Claude's constant reproduces `matches_prompt` (after RT-264); `inject.rs` waits on `ready_signal`; `presets.rs` formats prompt files with `file_reference`.

  Files: `pty_state.rs`, `server.rs` (watcher calls at reattach and spawn), `inject.rs`, `presets.rs`, `agent-claude` rules module. Proof: the `matches_prompt` cases move to `-p agent-claude rules` unchanged; `-p daemon pty_state` against a test rule set with a `Regex` rule; a serde round trip of Claude's rules; `-p daemon inject`; `-p daemon presets`.
- [ ] **AP.5 History, recovery and import behind the seam.** `plan_recovery` asks the entry's plugin for `recovery_options` and `shell_resume`; `entry_from_tracer_log` offers each log to `import_tracer_run` with the plugin's `import_rev`; `orphan.rs` takes `liveness_tokens`; presets without an agent use `default_agent()`.

  Files: `history.rs`, `server.rs` (`recover_one`), `orphan.rs`, `tracer_log.rs`, `presets.rs`, `agent-claude` recovery and import modules. Proof: `-p daemon history`, `-p daemon recover`, `-p agent-claude recovery`, `-p agent-claude import`, `-p daemon orphan`.
- [ ] **AP.6a The new wire types.** In `protocol`: `AgentSelection`, `Agents`, `AgentSettings`, `SetAgentSettings`, `agent_chips`, `conversation_id`, the new `RecoverAs`, `AgentId` replacing the `Agent` enum on the wire, `Option<AgentId>` for shells; the removals listed under Wire protocol, the v18 deserializers among them; `validate` (already in `agent-api`) wired into the daemon; `protocol-version.json` to 25 after the retire-path check.

  The brief ends with `-p protocol` and `-p agent-api` green; it lands with AP.6b.

  Files: `crates/protocol/src/lib.rs`, `protocol-version.json`, `agent-api` (validator). Proof: `-p protocol` round trips of every new message and type, `-p agent-api validate` (unknown agent, unknown key, bad choice, missing default written, hidden value dropped), `-p daemon-client supervisor`.
- [ ] **AP.6b Daemon on the new wire.** Validation in `spawn_session` and `SetAgentSettings`; `agent_settings` in `state.rs`; Claude's chip and texts; `agent_chips` and `conversation_id` on snapshots, sidecars and history; `Agents` and `AgentSettings` after `Welcome`; the new recovery kinds; the converter from AP.1b goes. The client gets only the edits it needs to compile: its Claude widgets read and write the `permission_mode`, `provider` and `model` keys until AP.7 and AP.8 replace them.

  Files: `server.rs`, `session.rs`, `orphan.rs`, `history.rs`, `state.rs`, `presets.rs`, `agent-claude` descriptor, the client files that fail to compile. Proof: `-p daemon` (a spawn with a bad option refuses naming the field; `SetAgentSettings` round trip and broadcast; a client receives `Agents` and `AgentSettings` before `Sessions`; a shell's snapshot has no agent), `--test ui ui_spawn_dialog`, then the full suite (the protocol is central in `tools/test-map.json`).
- [ ] **AP.7 Generic options in the spawn dialog.** `agent_options.rs` and `agent_options_view.rs`; `spawn_form.rs` and `spawn_view.rs` render the selected agent's fields from its settings and drop the Claude tables and checks listed under Client rendering; Runtime is built from `Agents`; the shell dialog sends no agent.

  Files: `agent_options.rs`, `agent_options_view.rs` (new), `spawn_form.rs`, `spawn_view.rs`, `shell_dialog.rs`, `session_menu.rs` (prefill), `launch_last.rs`, `connection.rs`, `apps/native/tests/support/mod.rs` (the fake daemon sends `Agents` with a `test-agent` descriptor and its settings), new spec `tests/ui/ui_agent_options.rs`. Proof: `agent_options` unit tests (rules, defaults, trusted lock, Unknown kind), `--test ui ui_agent_options`, `ui_spawn_dialog`, `ui_launch_last`.
- [ ] **AP.8 Settings pages, chips, labels and recovery in the client.** One Settings page per descriptor over `AgentSettings` / `SetAgentSettings`, and `SpawnDefaults` loses its Claude fields; chips from `agent_chips`; runtime tag, accent and Shift+Enter from the descriptor; the recover dialog's kinds from `caps` and labels from `texts`.

  Files: `settings_view.rs`, `sidebar.rs`, `sidebar_view.rs`, `grid_view.rs`, `term_input.rs`, `recover.rs`, `recover_view.rs`, `quit.rs`, `examples/shot.rs`. Proof: `--test ui ui_settings`, `ui_panes`, `ui_sidebar`, `ui_recover`, `ui_terminal`; before and after `.\rt.ps1 native-shot settings` and `spawn`.
- [ ] **AP.9 Fake CLI, the seam check, docs.** `tools/e2e/fake-claude/` moves to `crates/agents/claude/fake/` behind `fake_cli`; `live.rs` sets each registered plugin's override to its fake through `agent-registry`; the UI fixtures holding `"agent": "claude"` (inventory group 8, ten files) use `test-agent`; non-behaviour hits are renamed or allow-listed (Testing); `tools/check-agent-names.ps1`, its allow file and its prek hook; the docs below.

  Files: the fake (moved), `live.rs`, `tests/support/mod.rs`, the ten fixture files, the test keys in `env_secrets.rs`, `pty.rs`, `tracer_client.rs` and `protocol/src/env_rows.rs`, `tools/check-agent-names.ps1`, `tools/agent-names-allow.txt`, `prek.toml`, `tools/test-map.json`, `docs/architecture.md`, CLAUDE.md, `docs/native-client.md`, README glossary.

  Proof: the check passes, and fails on a planted `Claude` in `server.rs`; `.\rt.ps1 test -Validate`; `.\rt.ps1 native-e2e`; the full suite before the merge.

Docs in AP.9: architecture.md gains a Task Index row "Add or change an agent plugin" (`crates/agents/api` → the agent crate → `crates/agents/registry` → `protocol` schema types → `apps/native/src/agent_options.rs`), loses the agent-file lists in its spawn, hook and history rows, and lists the three crates in Components.

The glossary gains Agent plugin, Agent descriptor, Option schema and Seam check; CLAUDE.md's "Architecture invariants" gains the seam rule. Once nothing open links this file, its decisions move into `docs/agent-plugins.md` and this file is deleted.

## Testing

**Per-plugin tests** live in the agent crate and need no daemon: argv for each spawn shape, the environment plan, hook plans and the mapping of recorded hook payloads, the screen rules over recorded screens, the stream parser over recorded lines, transcript reading over fixture folders, recovery options and the tracer-log import. They run with `-p agent-claude`, mapped by a `crates/agents/claude/**` row in `tools/test-map.json`.

**The conformance test.** `agent-api`'s `test-support` feature exposes `conformance::check::<A: Agent>(&A)`: every field's default, every choice of every Choice field, each `when` rule's override and the empty map are validated by the app's schema function and then decoded into `A::Options`, and the struct's own serialization re-validates. Each agent crate calls it in one test, so a schema and its struct cannot drift.

**App tests** use `agent-api`'s `TestAgent` (an id, a schema, scripted argv, hook mapping and conversation plans), so daemon and client tests prove the generic paths, the pre-create runner and the discovery poller included, without naming Claude. Claude-specific daemon assertions (DeepSeek routing, `--settings`, `--resume`) move into `agent-claude`.

**The fake CLI.** Each plugin ships its own fake beside it (`crates/agents/claude/fake/`, today's `index.mjs` and `.cmd`), returned by `fake_cli` under `test-support`. The live harness iterates `agent-registry` and sets each plugin's `program_env_var` to its fake, so a later plugin brings its own fake without touching the harness. The live specs name "the first registered agent", never Claude.

**The seam check.** `tools/check-agent-names.ps1`: `rg -i` for `claude|anthropic|deepseek` over `crates/`, `apps/` and `tools/`, excluding the agent crates under `crates/agents/` other than `api` and `registry`.

Every remaining hit must match `tools/agent-names-allow.txt`, which lists paths or `path:pattern` pairs with a reason each. It runs as a prek hook and from `rt.ps1 clippy`, and fails naming each file and line. A prek hook runs on every commit, which matters in `test mode: unstable`, where a landing runs only its reached tests.

Hits that are not agent behaviour are fixed cheaply or allowed:

- `ANTHROPIC_API_KEY` as a sample secret key in tests (`env_secrets.rs`, `pty.rs`, `tracer_client.rs`, `protocol/src/env_rows.rs:59`, `146`): renamed to a neutral key (`EXAMPLE_API_KEY`) where the test only needs a name matching the secret pattern.
- Comments naming Claude as an example in agent-neutral files (`crates/tracer/*`, `paths.rs:46-48`, `state.rs:128`, `scrollback.rs`, `keep_awake.rs`): reworded to "the agent" where they describe behaviour, else allowed by path.

## Open questions

None: AQ2 and AQ6 are owner rulings, AQ8 is superseded by the no-compatibility ruling, and the rest are orchestrator decisions (Rulings).

## Prior art

- **herdr** (Apache-2.0; `docs/research/herdr-comparison.md`): per-agent screen-detection manifests (row "Heuristic PTY status", 80) shape `ScreenRules` as data rather than code, so RT-74 can load them from files; `pane.report_agent` as the agent-neutral report contract (22, 84) matches keeping `/hook/{session_id}` and `apply_to_record` in the app while each plugin owns only its vocabulary.

  Herdr plugins are argv commands with terminal-pane UI only (33-35), which is why this design sends a typed schema instead.
- **ThinkTerm** (`docs/plans/borrowed-ideas.md:39-42`, RT-74): prompt rules in versioned per-agent data files with a user override (`mux/src/agent_status/manifests/claude.toml`), the model for the deserializable `ScreenRules`.
- **The existing `AgentBackend`** (`crates/daemon/src/agents/mod.rs:165-245`): the trait AP.1a starts from; its header's claim that `server.rs` has no per-agent branching is not true (inventory, DOC DRIFT), which is what the seam check enforces.
- **VS Code contribution points**: an extension declares its settings as a JSON schema in its manifest and the editor renders the Settings UI from it, with no per-extension UI code; the descriptor's `fields` follow that split between a declared schema and a host-drawn page.
