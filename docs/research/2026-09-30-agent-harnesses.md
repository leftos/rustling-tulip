# Terminal agent harnesses for the spawn dialog's runtime dropdown

Researched 2026-09-30. Each fact cites its primary source (vendor docs, the repo's source or README at the version named). Anything taken only from a secondary source, or inferred, is marked **unverified**. "Fits the backend" refers to `crates/daemon/src/agents/` as of `de9c1ae`: a backend supplies a program name and env override, interactive argv (cwd, extra dirs, model, permission mode, initial prompt or the prompt injector, resume id), an optional headless argv and line parser, and the session's own conversation id, either chosen up front (Claude `--session-id`), pre-created by a separate run (Cursor `create-chat`), or found on disk after the fact (Codex rollout scan).

## Adoption evidence

npm weekly downloads (`api.npmjs.org/downloads/point/last-week/<pkg>`, fetched 2026-09-30), for scale only: `@openai/codex` 25.65M, `@anthropic-ai/claude-code` 14.50M, `@earendil-works/pi-coding-agent` 4.32M (+ 1.25M on the old `@mariozechner/pi-coding-agent`; Pi is also embedded as an SDK, so this overstates CLI users), `opencode-ai` 2.87M, `@github/copilot` 1.47M, `@google/gemini-cli` 450k, `cline` 98k, `@qwen-code/qwen-code` 84k, `@kilocode/cli` 48k, `@ampcode/cli` 40k (+ 23k on `@sourcegraph/amp`), `@moonshot-ai/kimi-code` 38k, `droid` 9.9k, `@charmland/crush` 9.2k. Antigravity CLI and Kiro CLI ship their own installers, not npm, so they have no comparable figure. GitHub stars (2026-09-30): opencode 211k, Gemini CLI 107k, Pi 111k, Cline 70k, Goose 55k, Aider 49k, Crush 28k, Qwen Code 28k.

## Support matrix

"Own id" is how a supervisor learns the conversation id of a fresh interactive spawn. "Choose" means a flag sets the id up front.

| Harness | Binary | Windows | Initial prompt (TUI) | Extra folders | Resume by id | Own id at spawn | Headless JSON | Auto-approve flag | Status signal | Account |
|---|---|---|---|---|---|---|---|---|---|---|
| Pi | `pi` | native (Git Bash) | positional | none | `--session <id>` | **choose**: `--session-id <id>` | `--mode json`, `--mode rpc` | none (no prompts by design) | extensions (unverified names) | own provider keys or `/login` |
| GitHub Copilot CLI | `copilot` | native | `-i <prompt>` | `--add-dir` | `--resume=<id>` | **choose**: `--session-id <uuid>` | `-p … --output-format=json` | `--allow-all` / `--yolo` | hooks (`agentStop`, …) | Copilot subscription |
| Gemini CLI | `gemini` | native | `-i <prompt>` | `--include-directories` | `--resume <uuid>` | **choose**: `--session-id <id>` | `-p … -o json\|stream-json` | `--yolo`, `--approval-mode` | hooks (`AfterAgent`, `Notification`, …) | Google account or API key |
| Qwen Code | `qwen` | native | `-i <prompt>` | `--include-directories` | `--resume <id>` | **choose**: `--session-id <id>` | `-p … --output-format json\|stream-json` | `--yolo`, `--approval-mode` | hooks (unverified events) | own provider keys |
| Goose | `goose` | native | `goose run -s -t <prompt>` | none | `goose session -r -n <name>` / `--session-id <id>` | choose a **name** (`-n`); id is generated | `goose run --output-format json\|stream-json` | `GOOSE_MODE=auto` (env) | unverified | own provider keys |
| Amp | `amp` | **WSL only** | stdin pipe only | none | `amp threads continue <id>` | pre-create: `amp threads new` (unverified output) | `-x --stream-json` | `--dangerously-allow-all` | unverified | Amp account (paid) |
| Antigravity CLI | `agy` | native | `-i` / `--prompt-interactive` | `/add-dir` in TUI; `--add-dir` flag unverified | `--conversation <id>` | status-line/title script payload, or transcript dir | `-p … --output-format json\|stream-json` | `--dangerously-skip-permissions` | status-line `agent_state`; hooks | Google account or `GEMINI_API_KEY` |
| Factory Droid | `droid` | native | positional | none documented | `droid --resume <id>` | `SessionStart` hook `session_id`, or transcript file | `droid exec -o json\|stream-json` | `--skip-permissions-unsafe` (exec), `--auto` | hooks (`Stop`, `Notification`) | Factory account |
| Kimi Code CLI | `kimi` | native (Git Bash) | none | `--add-dir` | `--session <id>` | `session_index.jsonl` by `workDir` | `-p … --output-format stream-json` | `--yolo`, `--auto` | hooks (`Stop`, `PermissionRequest`, …) | Kimi account or own keys |
| Kiro CLI | `kiro-cli` | native | positional `INPUT` | none documented | `--resume-id <id>` | `--list-sessions` per folder (format unverified) | `--no-interactive --output-format stream-json` (V2/V3) | `--trust-all-tools` | terminal title progress; hooks unverified | Kiro / AWS account |
| OpenCode | `opencode` | native | `--prompt` | none | `--session <id>` | `opencode session list --format json` | `opencode run --format json` | `--auto` | plugin events (`session.idle`, `permission.asked`) | own provider keys |
| Crush | `crush` | native | none | none | `--session <id>` | `crush session last --json` | none (`crush run` is text) | `--yolo` | hooks (preliminary) | own provider keys |
| Cline CLI | `cline` | native | positional with `-i` (unverified) | none | `--id <id>` | `cline history` | `--json` | `--auto-approve`, `--yolo` | hooks (`--hooks-dir`) | Cline account or own keys |

## Antigravity CLI (Google)

**Status.** Antigravity is not IDE-only. Google ships the **Antigravity CLI**, a terminal TUI launched as `agy`, alongside the Antigravity 2.0 desktop app; both run "the exact same agent core" and share settings ([CLI overview](https://antigravity.google/docs/cli/overview/)). Repo [google-antigravity/antigravity-cli](https://github.com/google-antigravity/antigravity-cli) holds the README, changelog and examples, not source; latest release `1.2.14`, published 2026-09-30. Licence: none on the repo; use is under the Google Terms of Service, with interaction data collected unless opted out (README "Terms of Service & Data Use").

**Relation to Gemini CLI.** The launch post says "the Antigravity CLI took inspiration from core Gemini CLI product and harness components" as part of consolidating "a single agent harness across Google-built developer surfaces", and links a migration guide from Gemini CLI ([blog, 2026-05-19](https://antigravity.google/blog/introducing-google-antigravity-cli)). Gemini CLI is not archived: `google-gemini/gemini-cli` released `v0.62.0` on 2026-09-29. Both share `~/.gemini/` (Antigravity uses `~/.gemini/antigravity-cli/` and `~/.gemini/config/`).

1. **Install.** `irm https://antigravity.google/cli/install.ps1 | iex` (PowerShell), a `.cmd` installer, or `curl … install.sh | bash`; "runs natively on macOS, Linux, and Windows" ([install](https://antigravity.google/docs/cli/install/)). Binary `agy`.
2. **Folder.** Run `agy` in the project folder; first launch asks to trust the workspace ([getting started](https://antigravity.google/docs/cli/getting-started)). Extra folders: `/add-dir <path>` slash command ([reference](https://antigravity.google/docs/cli/reference/)). A `--add-dir` launch flag appears only in secondary cheat sheets: **unverified**.
3. **Initial prompt.** `--prompt-interactive` exists (changelog: "the `--print` and `--prompt-interactive` conflict"); `-i` short form **unverified**.
4. **Resume.** `--conversation <id>` resumes by id, `--continue`/`-c` the most recent ([headless](https://antigravity.google/docs/cli/headless/), shown with `-p`; interactive use of `--conversation` **unverified**, though the CLI prints "the exact command needed to resume" on exit, [using](https://antigravity.google/docs/cli/using/)). Id capture: a custom status-line or title script (`settings.json` `"statusLine"`/`"title": {"type":"command",…}`) receives JSON on every state change with `conversation_id`, `transcript_path` (example `~/.gemini/antigravity/brain/<id>/.system_generated/logs/transcript.jsonl`) and `agent_state` ([status line](https://antigravity.google/docs/cli/statusline/), [title](https://antigravity.google/docs/cli-title)). Whether these can be set per launch rather than in the user's `settings.json`: **unverified**.
5. **Headless.** `agy -p "<prompt>" --output-format json|stream-json`; stream starts with `init` carrying `conversation_id`; `--input-format stream-json` keeps one process for many turns ([headless](https://antigravity.google/docs/cli/headless/)).
6. **Model / permissions.** `--model <slug>` (`agy models` lists), `--effort`, `--agent`, `--mode=accept-edits|plan` ([modes](https://antigravity.google/docs/cli/modes/)), `--sandbox`, `--dangerously-skip-permissions`.
7. **Status.** `agent_state` values `idle`, `thinking`, `working`, `tool_use`, `initializing`, plus `tool_confirmation_pending` ([status line](https://antigravity.google/docs/cli/statusline/)). Hooks via `~/.gemini/config/hooks.json` and `<workspace>/.agents/hooks.json` with `PostToolUse`, `Stop`, `PostInvocation` events (changelog).
8. **Account.** Google sign-in through the OS keyring, or `GEMINI_API_KEY` with `modelProvider: "gemini"` ([install](https://antigravity.google/docs/cli/install/)).

## Pi (`pi`, earendil-works/pi)

Repo [earendil-works/pi](https://github.com/earendil-works/pi) (formerly `badlogic/pi-mono`), MIT, release `v0.99.2` (2026-09-30). Docs cited at that tag under `packages/coding-agent/docs/`.

1. **Install.** `npm install -g --ignore-scripts @earendil-works/pi-coding-agent` (Node ≥ 22.19) ([README](https://github.com/earendil-works/pi/blob/v0.99.2/packages/coding-agent/README.md)). Windows: native, using Git Bash for its `bash` tool, optional `powershell` tool; WSL also works ([windows.md](https://github.com/earendil-works/pi/blob/v0.99.2/packages/coding-agent/docs/windows.md)).
2. **Folder.** Working directory = process cwd; it "controls project configuration, resource discovery, and session grouping". No extra-folder flag ([cli.md](https://github.com/earendil-works/pi/blob/v0.99.2/packages/coding-agent/docs/cli.md)).
3. **Initial prompt.** `pi [options] [@files...] [messages...]`: a positional message opens the TUI with it as the first prompt (cli.md).
4. **Resume.** `--session-id <id>` "opens the exact project session ID or creates it if absent" (letters, digits, `.`, `_`, `-`); `--session <path|id>` opens by id; `-c` continues the latest. Sessions are stored under `~/.pi/agent/sessions/`, grouped by working directory; `--session-dir` / `PI_CODING_AGENT_SESSION_DIR` override ([sessions.md](https://github.com/earendil-works/pi/blob/v0.99.2/packages/coding-agent/docs/sessions.md)).
5. **Headless.** `-p` (text), `--mode json` (JSONL events), `--mode rpc` (JSONL commands on stdin) (cli.md).
6. **Model / permissions.** `--model <pattern>[:thinking]`, `--provider`, `--thinking`. Pi has no approval prompts at all: "it does not ask for approval before every tool call" ([security.md](https://github.com/earendil-works/pi/blob/v0.99.2/packages/coding-agent/docs/security.md)). `-a`/`--approve` only trusts project-local config for the run. `--append-system-prompt` exists (useful for the workspace prelude).
7. **Status.** No hooks file; TypeScript extensions receive agent events. Event names **unverified**.
8. **Account.** Bring your own provider (`/login` for a subscription or key).

## GitHub Copilot CLI

Repo [github/copilot-cli](https://github.com/github/copilot-cli) (docs and issues; binary is closed), release `v1.0.90` (2026-09-30). Flags from the [command reference](https://docs.github.com/en/copilot/reference/cli-command-reference) ([source md](https://github.com/github/docs/blob/main/content/copilot/reference/copilot-cli-reference/cli-command-reference.md)).

1. **Install.** `winget install GitHub.Copilot` or `npm install -g @github/copilot`; Windows needs PowerShell 6+ (README). Binary `copilot`.
2. **Folder.** `-C <dir>`; `--add-dir=PATH`, repeatable.
3. **Initial prompt.** `-i PROMPT` / `--interactive=PROMPT`.
4. **Resume.** `--session-id ID`: resumes a matching session, otherwise "a new session is created only when the value is a valid UUID" — one flag for fresh and resume. `--resume=<id|prefix|name>`, `--continue`. State lives under `COPILOT_HOME` (default `~/.copilot`); the session-state subfolder name is **unverified**.
5. **Headless.** `-p PROMPT --output-format=json` (JSONL); `--allow-all-tools` is "required when using the CLI programmatically".
6. **Model / permissions.** `--model=MODEL` (or `auto`), `--mode=interactive|plan|autopilot`, `--plan`, `--autopilot`, `--allow-all`/`--yolo`, `--allow-tool`/`--deny-tool`.
7. **Status.** Hooks (`command`/`http`/`prompt` types) with events including `agentStop` and `errorOccurred` ([hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)).
8. **Account.** Active Copilot subscription (README); BYOK providers via `COPILOT_PROVIDERS_CONFIG`.

## Gemini CLI

Repo [google-gemini/gemini-cli](https://github.com/google-gemini/gemini-cli), Apache-2.0, `v0.62.0` (2026-09-29). Flags from [`packages/cli/src/config/config.ts`](https://github.com/google-gemini/gemini-cli/blob/v0.62.0/packages/cli/src/config/config.ts).

1. **Install.** `npm install -g @google/gemini-cli`; native on Windows (Node). Binary `gemini`.
2. **Folder.** cwd; `--include-directories a,b` (repeatable); `--skip-trust` trusts the workspace for the session.
3. **Initial prompt.** `-i/--prompt-interactive`; a bare positional query also starts interactive.
4. **Resume.** `--session-id <id>` "start a new session with a manually provided UUID"; `--resume <uuid|index|latest>`. Sessions in `~/.gemini/tmp/<project_hash>/chats/` ([session-management.md](https://github.com/google-gemini/gemini-cli/blob/v0.62.0/docs/cli/session-management.md)).
5. **Headless.** `-p <prompt> -o json|stream-json`.
6. **Model / permissions.** `-m`, `--approval-mode default|auto_edit|yolo|plan`, `-y/--yolo`, `-s/--sandbox`, `--policy`.
7. **Status.** Hooks `BeforeAgent`, `AfterAgent`, `Notification`, `SessionStart`, `SessionEnd`, … ([hooks reference](https://github.com/google-gemini/gemini-cli/blob/v0.62.0/docs/hooks/reference.md)).
8. **Account.** Google login (free tier) or Gemini / Vertex API key (README).

## Qwen Code

Repo [QwenLM/qwen-code](https://github.com/QwenLM/qwen-code) (a Gemini CLI fork), Apache-2.0, npm `0.24.7` (2026-09-30). Flags from [`top-level-options.ts`](https://github.com/QwenLM/qwen-code/blob/main/packages/cli/src/config/top-level-options.ts) on `main`.

1. **Install.** `npm install -g @qwen-code/qwen-code`; Windows installer in README. Binary `qwen` (**unverified** from source; README usage).
2. **Folder.** cwd; `--include-directories`.
3. **Initial prompt.** `-i/--prompt-interactive`.
4. **Resume.** `--session-id <id>` ("Specify a session ID for this run"), `-r/--resume <id>`, `-c`, `--fork-session`. On-disk location **unverified**.
5. **Headless.** `-p`, `--output-format text|json|stream-json`, `--input-format`, `--include-partial-messages`.
6. **Model / permissions.** `--model`, `--yolo`, `--approval-mode`, `--sandbox`.
7. **Status.** README lists Hooks as supported; event names **unverified**.
8. **Account.** `/auth` to configure a provider and API key (README).

## Goose

Repo [aaif-goose/goose](https://github.com/aaif-goose/goose) (moved from `block/goose`), Apache-2.0, `v1.52.0` (2026-09-23). Flags from the [CLI commands guide](https://goose-docs.ai/docs/guides/goose-cli-commands/).

1. **Install.** `download_cli.sh` or a PowerShell script; native Windows needs Git Bash, MSYS2 or PowerShell ([installation.md](https://github.com/aaif-goose/goose/blob/main/documentation/docs/getting-started/installation.md)). Binary `goose`.
2. **Folder.** cwd. No extra-folder flag.
3. **Initial prompt.** `goose run -s -t "<prompt>"` runs the prompt then stays interactive.
4. **Resume.** `goose session -n <name>` names a session; `goose session -r -n <name>` or `-r --session-id <id>` resumes. Ids are generated (`20251108_1`). Whether reusing a name without `-r` collides: **unverified**.
5. **Headless.** `goose run --output-format json|stream-json -t …`.
6. **Model / permissions.** `--provider`, `--model`; `GOOSE_MODE` = `auto` (default), `approve`, `smart_approve`, `chat` ([config-files.md](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/config-files.md)).
7. **Status.** **unverified**.
8. **Account.** Own provider keys.

## Amp

Closed source (Amp Inc., formerly Sourcegraph). npm `@ampcode/cli` `0.0.1790827300-gda351f` (2026-10-01).

1. **Install / Windows.** "The CLI supports macOS, Linux, and Windows through WSL" ([CLI docs](https://ampcode.com/docs/cli)). A third-party integrator reports the npm package runs on Windows ([Daintree](https://daintree.org/docs/agents/amp)): **unverified** by Amp.
2. **Folder.** cwd. No extra-folder flag documented.
3. **Initial prompt.** Only piped stdin becomes the first message in interactive mode (CLI docs) — use the prompt injector.
4. **Resume.** `amp threads continue <threadId>`; `amp threads new` creates a thread ([amp-x news](https://ampcode.com/news/amp-x), [examples repo](https://github.com/ampcode/amp-examples-and-guides/blob/main/guides/cli/README.md)). That `threads new` prints the id for capture: **unverified**. Threads sync to ampcode.com.
5. **Headless.** `amp -x "<prompt>" --stream-json`; `init` and `result` carry `session_id` (`T-<uuid>`) ([streaming JSON](https://ampcode.com/docs/cli/streaming-json)).
6. **Model / permissions.** `--dangerously-allow-all`; `--executor local|orb|runner:<id>`. Model flag **unverified** (Amp picks models).
7. **Status.** **unverified**.
8. **Account.** Amp account required.

## Factory Droid

Closed source; npm `droid` `0.230.0` (2026-09-30). [CLI reference](https://docs.factory.ai/reference/cli-reference).

1. **Install.** `irm https://app.factory.ai/cli/windows | iex` or npm; Windows binary `droid.exe`.
2. **Folder.** cwd (`--cwd` for exec). No extra-folder flag documented.
3. **Initial prompt.** `droid "query"`.
4. **Resume.** `droid --resume [sessionId]`, `--fork <id>`. Id capture: `SessionStart` hook input carries `session_id` and `transcript_path` `~/.factory/projects/…/<uuid>.jsonl` ([hooks](https://docs.factory.ai/harness/hooks)).
5. **Headless.** `droid exec -o json|stream-json|stream-jsonrpc`, `-s <id>` to continue.
6. **Model / permissions.** `-m`, `--auto low|medium|high`, `--skip-permissions-unsafe`, `--use-spec`, `--append-system-prompt`. Which of these the interactive `droid` accepts (vs `exec` only) is not split out in the table: **unverified**.
7. **Status.** Hooks `Stop`, `Notification` (`permission_prompt`, `idle_prompt`), `SessionStart`, `SessionEnd`, in `~/.factory/hooks.json` or `.factory/hooks.json`.
8. **Account.** Factory account (BYOK models available).

## Kimi Code CLI

Repo [MoonshotAI/kimi-code](https://github.com/MoonshotAI/kimi-code), MIT, `2.1.1` (2026-09-24). It replaces the Python Kimi CLI, archived and "no longer maintained" ([kimi-cli README](https://github.com/MoonshotAI/kimi-cli)). [`kimi` command reference](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/reference/kimi-command.md).

1. **Install.** `irm https://code.kimi.com/kimi-code/install.ps1 | iex` or `npm i -g @moonshot-ai/kimi-code`; Windows needs Git for Windows (README). Binary `kimi`.
2. **Folder.** cwd; `--add-dir <dir>`, repeatable.
3. **Initial prompt.** None for the TUI (`-p` is non-interactive) — use the prompt injector.
4. **Resume.** `-S/--session <id>`, `-c`. Sessions under `~/.kimi-code/sessions/<workDirKey>/<sessionId>/`, indexed by `session_index.jsonl` with `sessionId`, `sessionDir`, `workDir` ([data-locations.md](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/configuration/data-locations.md)).
5. **Headless.** `-p <prompt> --output-format stream-json`.
6. **Model / permissions.** `-m <alias>`, `--yolo` (ask when needed), `--auto` (never ask), `--plan`.
7. **Status.** Hooks incl. `Stop`, `TurnStarted`, `PermissionRequest`, `SessionStart` ([hooks.md](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/customization/hooks.md)).
8. **Account.** Kimi login or own providers.

## Kiro CLI

Closed source (AWS). [CLI commands](https://kiro.dev/docs/reference/cli-commands/) (page dated 2026-09-12).

1. **Install / Windows.** Terminal UI "supported on macOS, Linux … and Windows" ([terminal UI](https://kiro.dev/docs/cli/terminal-ui/)). Binary `kiro-cli`. Install command **unverified**.
2. **Folder.** cwd; sessions are stored per directory. No extra-folder flag documented.
3. **Initial prompt.** `kiro-cli chat [INPUT]`.
4. **Resume.** `--resume-id <id>`, `-r` (latest in folder), `--list-sessions`. Sessions are UUIDs in a local database, "not files" ([session management](https://kiro.dev/docs/cli/chat/session-management/)). Machine-readable listing **unverified**.
5. **Headless.** `--no-interactive`, `--output-format stream-json` (V2/V3 engines) ([headless](https://kiro.dev/docs/cli/headless/)).
6. **Model / permissions.** `--trust-all-tools`, `--trust-tools`, `--agent`, `--effort`; a `--model` flag is **unverified** (`--list-models` exists).
7. **Status.** The terminal title "reflects agent state (streaming, pending approval, error)" (terminal UI page).
8. **Account.** Kiro sign-in (**unverified** which providers).

## OpenCode

Repo [anomalyco/opencode](https://github.com/anomalyco/opencode) (moved from `sst/opencode`), MIT, `v1.18.34` (2026-09-30). [CLI docs](https://opencode.ai/docs/cli/).

1. **Install.** `scoop install opencode`, `choco install opencode`, or `npm i -g opencode-ai` (README). Binary `opencode`.
2. **Folder.** `opencode [project]`. No extra-folder flag.
3. **Initial prompt.** `--prompt <text>`.
4. **Resume.** `-s/--session <id>`, `-c`, `--fork`. Sessions live in a database (`opencode db path`); `opencode session list --format json` lists them. Whether the JSON carries the directory for matching a spawn: **unverified**.
5. **Headless.** `opencode run --format json`; `opencode serve` HTTP API.
6. **Model / permissions.** `-m provider/model`, `--agent`, `--auto`, `OPENCODE_PERMISSION` (inline JSON).
7. **Status.** Plugin events `session.created`, `session.idle`, `session.status`, `permission.asked` ([plugins](https://opencode.ai/docs/plugins/)).
8. **Account.** Own provider keys (`opencode auth login`).

## Crush

Repo [charmbracelet/crush](https://github.com/charmbracelet/crush), FSL-1.1-MIT, `v0.97.1` (2026-09-29). Flags from [`internal/cmd/root.go`](https://github.com/charmbracelet/crush/blob/v0.97.1/internal/cmd/root.go) and `run.go`.

1. **Install.** `winget install charmbracelet.crush` or Scoop; Windows PowerShell and WSL supported (README).
2. **Folder.** `-c/--cwd`. No extra-folder flag.
3. **Initial prompt.** None for the TUI — prompt injector.
4. **Resume.** `-s/--session <id>`, `-C/--continue`; `crush session list|last|show --json`.
5. **Headless.** `crush run "<prompt>"` text only; no JSON output flag.
6. **Model / permissions.** `-y/--yolo`; `-m` on `run` only.
7. **Status.** "Preliminary support for hooks" (README).
8. **Account.** Own provider keys.

## Cline CLI

Repo [cline/cline](https://github.com/cline/cline) (`apps/cli`), Apache-2.0, npm `cline` `3.0.67` (2026-09-30). [CLI reference](https://docs.cline.bot/cli/cli-reference).

1. **Install.** `npm i -g cline`; supports macOS, Linux and Windows ([overview](https://github.com/cline/cline/blob/main/docs/cline-cli/overview.mdx)).
2. **Folder.** `-c/--cwd`. No extra-folder flag.
3. **Initial prompt.** `cline "task"` with a TTY opens the UI; with the current `-i/--tui` split, whether a positional prompt plus `-i` stays interactive is **unverified**.
4. **Resume.** `--id <session-id>`; `cline history` lists sessions. Id capture **unverified**.
5. **Headless.** `--json` (NDJSON `ask`/`say` messages).
6. **Model / permissions.** `-P` provider, `-m` model, `--auto-approve true|false` (default `true`), `-y/--yolo`, `-p/--plan`.
7. **Status.** Hooks via `--hooks-dir` (default `~/.cline/hooks`) and `cline hook`; events **unverified**.
8. **Account.** Cline provider by default, or own keys (`-k`).
