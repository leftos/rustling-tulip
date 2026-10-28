# herdr vs rustling-tulip: could rustling-tulip be herdr plugins?

Researched 2026-10-13, against herdr v0.9.4 (released 2026-10-13). Herdr doc pages were read from their source in the repo (`docs/next/website/src/content/docs/*.mdx` at `HEAD` of [herdrdev/herdr](https://github.com/herdrdev/herdr)) and are cited by their published URL under `https://herdr.dev/docs/`. GitHub figures (stars, contributors, releases) come from the GitHub API on the same day.

## 1. What herdr is

Herdr is an Apache-2.0, single-binary Rust **terminal workspace manager** for coding agents: a background server owns the PTYs (ConPTY on Windows), and a TUI client, rendered inside whatever terminal you already use, attaches to it tmux-style with workspaces, tabs, split panes, mouse support and an agent sidebar ([README](https://github.com/herdrdev/herdr/blob/master/README.md), [Concepts](https://herdr.dev/docs/concepts/)).

It detects about 22 agent CLIs (Claude Code, Codex, Cursor Agent, opencode, Copilot, Grok, …) and marks each pane `working`/`blocked`/`done`/`idle` from screen-detection manifests, or from the agent's own reports where an integration or the agent supports it ([Agents](https://herdr.dev/docs/agents/)).

Everything is driven through a CLI and a JSON socket API that agents themselves use ([Socket API](https://herdr.dev/docs/socket-api/)). Remote machines are attached over SSH ([Connecting machines](https://herdr.dev/docs/connecting-machines/)).

The repo was created 2026-03-27, has 43,869 stars, 89 contributors, 60 stable releases plus preview builds, and ships roughly weekly (v0.9.1 2026-09-16, v0.9.2/0.9.3 2026-09-29, v0.9.4 2026-10-13) ([releases](https://github.com/herdrdev/herdr/releases)).

The site claims 1,696 community plugins and 1.48M installs ([herdr.dev](https://herdr.dev)); the install count is unverified. Native Windows is "generally available", but plugins on Windows are "preview" ([Windows support](https://herdr.dev/docs/windows-beta/)).

### Key facts that decide the comparison

- **Process survival.** Detach keeps processes alive; a **server restart kills every pane process**. Afterwards Herdr restores the layout, can replay pane history (off by default, `experimental.pane_history`), and resumes supported agents with their native resume command (`claude --resume <id>`, `codex resume <id>`, `cursor-agent --resume <id>`) ([Session state](https://herdr.dev/docs/session-state/)).

  Live handoff (old server passes PTYs to the new one) is experimental, opt-in, and **unsupported on Windows** ([Windows support](https://herdr.dev/docs/windows-beta/), "Not supported on Windows"). rustling-tulip's per-session `rt-tracer.exe` keeps the ConPTY and child alive across daemon restarts on Windows; herdr has no equivalent there.
- **Claude Code status is screen-scraped.** The Claude integration is a `SessionStart` hook that reports session identity only; "Claude Code state comes from Herdr's screen manifest detection" ([Integrations](https://herdr.dev/docs/integrations/)). Codex's hook does report turn start/stop. Any agent or hook can push state with `pane.report_agent` ([Socket API](https://herdr.dev/docs/socket-api/), "Agent state reporting").
- **Worktrees are core.** `worktree.create/open/remove` are socket methods and `worktree.created` is a plugin event ([Socket API](https://herdr.dev/docs/socket-api/)). Multi-repo workspaces with a shared worktree anchor and `--add-dir` (rustling-tulip's model) are not documented; a herdr "workspace" is a container for tabs, usually one repo ([Concepts](https://herdr.dev/docs/concepts/)).
- **Remote is SSH only.** Saved machines and `herdr --remote` use the system OpenSSH client and install herdr on the host if needed ([Persistence and remote](https://herdr.dev/docs/persistence-remote/)). No TLS relay, pairing or per-device credentials in core.
- **Multiple clients and versions.** Several clients may attach, each viewing its own tab; client and server builds need not match since "endpoint generation 1" for the client-shell endpoint, though the JSON socket API's CLI and direct terminal attach still require an exact protocol match ([Concepts](https://herdr.dev/docs/concepts/), [Socket API](https://herdr.dev/docs/socket-api/), "Protocol stability").

### What a plugin can do

From [Plugins](https://herdr.dev/docs/plugins/) and [Socket API, Plugin APIs](https://herdr.dev/docs/socket-api/):

| Question | Answer |
|---|---|
| Language | Any argv command (Bash, Node, Python, Rust binary, …). No SDK: "the entire Herdr CLI is the plugin API." |
| Entry points | Manifest-only: `[[build]]`, `[[startup]]` (one-shot after restore, "rather than supervised daemons"), `[[actions]]`, `[[events]]` (e.g. `worktree.created`), `[[panes]]`, `[[link_handlers]]`. "Runtime action registration and runtime argv pane creation are not part of v1." |
| UI | Terminal panes only: `overlay`, `popup`, `split`, `tab`, `zoomed`. "Native non-terminal plugin UI [is] not part of plugin v1." No sidebar rows, badges or dialogs beyond a terminal pane; metadata tokens can be pushed with `pane.report_metadata` / `workspace.report_metadata` and shown in configurable sidebar rows ([Socket API](https://herdr.dev/docs/socket-api/), [Connecting machines](https://herdr.dev/docs/connecting-machines/)). |
| Sandboxing | None. Runs as the user with full CLI access; herdr "does not review or sandbox plugin code." |
| Processes | A plugin can spawn anything, but herdr does not supervise it; "processes surviving server exit are not protected from cleanup" and plugins "must stop detached background processes before their parent command or pane exits." A long-lived service must run as a pane or outside herdr. |
| PTY I/O | Read pane text and send input via CLI/API, subscribe to events (`pane.agent_status_changed`, lifecycle, output waits). No interception or rewriting of the byte stream. Event history "is not durable"; slow subscribers get `events_lost`. |
| Network endpoints | Not provided by herdr; a plugin process may open its own sockets (that is how the mobile/web plugins below work). |
| State | No storage API; plugins own files under `HERDR_PLUGIN_STATE_DIR`. |
| Where it runs | On the server's machine, launched by the server, inheriting `HERDR_SOCKET_PATH` (Unix socket or Windows named pipe). |

### Popular plugins and clients (GitHub topic `herdr-plugin`, by stars)

The marketplace is an unreviewed index of repos tagged `herdr-plugin` ([Marketplace](https://herdr.dev/docs/marketplace/)). Top entries relevant here ([GitHub topic search](https://github.com/topics/herdr-plugin)):

| Repo | Stars | What it does |
|---|---|---|
| [AltanS/collie](https://github.com/AltanS/collie) | 1,332 | Self-hosted PWA for iPhone/Android: drive Claude Code, Codex, … in herdr; push alerts |
| [furkankly/zoetrope](https://github.com/furkankly/zoetrope) | 1,034 | Watch a Claude Code or Codex session as a live flow graph |
| [devswha/herdr-web-ui](https://github.com/devswha/herdr-web-ui) | 924 | Browser and phone client: chat and live terminal per agent pane, SSH remotes, web push |
| [persiyanov/herdr-reviewr](https://github.com/persiyanov/herdr-reviewr) | 884 | Review an agent's diff in a pane, send line comments back to the agent |
| [eliasstravik/herdr-projects](https://github.com/eliasstravik/herdr-projects) | 662 | Coordinator conversation, parallel workers, shared memory, "what needs you" overview |
| [smarzban/herdr-file-viewer](https://github.com/smarzban/herdr-file-viewer) | 653 | Git-aware read-only file viewer with diffs |
| [ZingerLittleBee/Heeler](https://github.com/ZingerLittleBee/Heeler) | 572 | Native iOS console over SSH with push notifications |
| [alexarthurs/herdr-sidebar](https://github.com/alexarthurs/herdr-sidebar) | 468 | File explorer + git source control in one pane, AI commit messages |
| [dcolinmorgan/herdr-remote](https://github.com/dcolinmorgan/herdr-remote) | 418 | Menu bar, phone or Telegram control; free tunnel for remote |
| [0cv/herdr-mobile-relay](https://github.com/0cv/herdr-mobile-relay) | 296 | Phone approve/monitor via relay, push, QR setup |
| [nicosuave/memex](https://github.com/nicosuave/memex) | 261 | Search agent transcripts, resume sessions, track tokens |

Outside the topic: [penso/herdr-gpui](https://github.com/penso/herdr-gpui) (1,296 stars, unaffiliated) is a **native Rust/GPUI client for macOS, Linux and Windows** that paints the daemon's cells, with worktrees in the sidebar, a review tab with notes sent to the agent, and "Teleport" of a worktree to another host ([README](https://github.com/penso/herdr-gpui/blob/main/README.md)).

[missuo/herdrm](https://github.com/missuo/herdrm) is a native macOS console. None of these plugins were installed or tested; their behaviour, Windows support and quality are unverified.

## 2. Feature comparison

Legend: **Core** = herdr has it; **Plugin** = an existing plugin/client covers it; **New plugin** = feasible with plugin v1; **Core change** = needs herdr changes; **No** = ruled out by herdr's architecture.

| rustling-tulip capability | Status in RT | herdr |
|---|---|---|
| Daemon owns PTYs, multiple clients, client auto-spawns daemon | Shipped | **Core** (server + clients, auto-start) |
| Session processes survive a daemon restart (ConPTY tracer) | Shipped | **No on Windows**: restart kills panes; live handoff is Unix-only and experimental ([Session state](https://herdr.dev/docs/session-state/), [Windows](https://herdr.dev/docs/windows-beta/)). A plugin cannot own panes' PTYs, so not feasible as a plugin |
| Scrollback ring replayed on attach | Shipped | **Core** while the server lives; across restarts only with opt-in `pane_history` |
| Session history + resume (Claude, Codex, Cursor) | Shipped | **Core**: native agent session restore for 18 agents incl. all three |
| Claude / Codex / Cursor / shell sessions | Shipped | **Core** (22 agents detected) |
| Per-session git worktrees, single repo | Shipped | **Core** (`worktree.*`) |
| Multi-repo workspace sessions (shared anchor, `--add-dir`, relative paths preserved) | Shipped | **New plugin**, partly: a plugin can create the worktrees with git and open a pane running `claude --add-dir …`; herdr's own worktree model and sidebar would not know the group |
| Live git watcher + source control panel | Shipped | **Plugin** (herdr-sidebar, herdr-file-viewer) as terminal panes; GUI panel only in herdr-gpui |
| Native GPUI desktop client | Shipped | **Third-party client** (herdr-gpui); herdr core is a TUI. Not a plugin, but a client over herdr's API is possible |
| Heuristic PTY status | Shipped | **Core**, more mature (per-agent manifests, `agent.explain`) |
| W1a DeepSeek provider sessions | Planned | **New plugin**: an action/pane that launches `claude` with provider env vars |
| W1b Env secrets kept out of stored files and client messages | Planned | **Core change**: pane env passes through herdr's `session.json`/API; a plugin cannot control what herdr persists (unverified whether herdr persists pane env) |
| W1c Exclude a session from busy tracking | Planned | **Core change** or config (unverified) |
| W2 Hook-based Claude status, background state, hand-off flag | Planned | **New plugin**: Claude hooks calling `pane.report_agent` (the documented path for agents/hooks). Herdr core itself scrapes Claude's screen |
| W3 Attention feed, toasts with answers, sounds, badge, context %/cost | Planned | Partly **Core** (blocked/done states, system notifications and sounds); **New plugin** for a feed pane, toasts and cost; an "inbox" with unread state in the sidebar would need **Core change** (no plugin sidebar UI) |
| W4 Tabs, panes, tab state | Planned | **Core** |
| W5 Remote desktop over TLS relay / Tailscale, per-device credentials, file fetch, click-to-fetch paths | Planned | SSH remote is **Core**; relay/pairing exist as **Plugins** (herdr-remote, herdr-mobile-relay). Click-a-path-to-fetch over remote and per-device revocable credentials need **Core change** or a custom client |
| W6 Repos, containers, presets wizard | Planned | **New plugin** (pane-based wizard + `workspace.create`) |
| W7 Android app: pairing, feed, reply, push | Planned | **Plugin** (collie, herdr-web-ui, herdr-mobile-relay, Heeler for iOS) |
| W9 Pop-out windows, drag-and-drop | Planned | **No** in the TUI; client-side feature of a GUI client (herdr-gpui) |
| W10 Chat mirror with interactive cards | Planned | **Plugin** partly (herdr-web-ui "chat" view); a GUI chat in the desktop app needs a custom client |
| W11 Summaries, spoken alerts, dashboard | Planned | **New plugin** (subscribe to `pane.agent_status_changed`, read pane, call a summarizer, speak). Dashboard as a terminal pane only |
| W12 Subagent streams | Planned | **New plugin** (tail transcripts in a pane); zoetrope is adjacent |
| W13 Follow dispatch runs | Planned | **New plugin** |
| W14 Claude accounts, artifacts by account | Planned | **New plugin**; no account concept in core |
| W15 Gate dashboard, plan board, dock | Planned | **New plugin** as panes; dock bar **No** (TUI) |
| W16 Dictation into a session | Planned | **No** as a plugin in a TUI (needs OS key hooks and focus in the client process); feasible in a GUI client |
| Idea: agent drives other sessions | Idea | **Core** (agents spawn, prompt and wait on each other) |
| Idea: diff comments sent to an agent | Idea | **Plugin** (herdr-reviewr; herdr-gpui review tab) |
| Idea: connect to several daemons, grouped by host | Idea | **Core** (saved SSH machines) |

## 3. Plugin-boundary verdict

**Could be herdr plugins (plugin v1 is enough):** DeepSeek/provider launch profiles, hook-based Claude status (via `pane.report_agent`), presets and multi-repo worktree setup, spoken alerts and summaries, subagent and dispatch viewers, accounts switching, gate/plan views, attention feed as a pane. Most mobile/remote-relay goals are already covered by existing community plugins and clients. Cost: every UI is a terminal pane, nothing is supervised, and Windows plugin support is "preview".

**Need herdr core changes:** secret-aware env handling, plugin-owned sidebar UI (attention inbox, badges, unread state), per-device remote credentials and a TLS relay as first-class transport, multi-repo workspace awareness in herdr's worktree model, a durable event stream (current subscriptions drop with `events_lost`).

**Ruled out by herdr's architecture:**

- **Windows process survival across server restarts.** Herdr's server owns the ConPTYs directly; restart means processes die and agents resume from their conversation id. Live handoff is Unix-only. rustling-tulip's tracer design is the one thing herdr cannot give you on Windows today, and a plugin cannot provide it because plugins do not own pane PTYs.
- **Native GPUI client, dictation, pop-out windows, drag-and-drop, toasts with inline answers.** Herdr's own client is a TUI. These are possible only as a separate client over herdr's API, which herdr-gpui already demonstrates; that is "write a client", not "write a plugin", and you would depend on an API herdr calls stable at the endpoint generation level but still evolving (plugin API is "an early interface").
- **Byte-level PTY interception** (if any planned feature needs it): not exposed.

## 4. Recommendation

**Borrow ideas and test the restart gap; do not adopt herdr as the base yet.** Concretely:

1. **Measure the one decisive difference.** rustling-tulip's tracer survival matters only if daemon restarts are frequent (you rebuild the daemon during development; ordinary users rarely restart). With herdr, a server restart costs running shells, dev servers and in-flight agent turns; agents come back via `--resume` only between turns. If you can live with that, most of the rest is covered.
2. **Borrow from herdr now:** per-agent screen-detection manifests and `agent.explain` (feeds RT-74), `pane.report_agent` as the hook contract (RT-25), the agent-native CLI where agents spawn/prompt/wait on each other (RT-53), herdr's 18-agent resume table (RT-49, RT-57), the plugin manifest shape if rustling-tulip ever grows extensions, and the review-to-agent flow from herdr-reviewr / herdr-gpui (RT-72).
3. **Option tradeoffs.**
   - *Adopt herdr + write plugins:* you drop the daemon, tracer, protocol and remote work and inherit a fast-moving, 89-contributor project with mobile and GPUI clients already built. You lose Windows process survival, the native GPUI client becomes a third-party fork or your own client over herdr's API, plugin UI is terminal-only, and Windows plugins are preview. Your roadmap's center of gravity (desktop GUI, attention inbox, dictation, toasts) is client work that plugins cannot do.
   - *Write your own GPUI client over herdr's daemon* (what herdr-gpui does): keeps your native UI ambition, drops the daemon. Same restart loss; you track herdr's API.
   - *Contribute upstream:* a Windows tracer-style handoff, hook-driven Claude state, and plugin sidebar UI would be valuable upstream, but you would not control priorities.
   - *Stay the course:* keeps the Windows survival guarantee and full control, at the cost of rebuilding what herdr and its ecosystem already ship (remote, mobile, status detection breadth).
4. **Evidence that would change this.** Toward adopting: herdr shipping live handoff on Windows (or a tracer-like supervisor), Windows plugins leaving preview, plugin v2 with native UI hooks, or a week of daily use of herdr + herdr-gpui + collie on this Windows machine without missing anything from the off-Orca bar. Toward staying: herdr-gpui or the mobile plugins failing on Windows, herdr persisting secret env vars, or restart frequency in your workflow being high.

Unverified: plugin behaviour and Windows quality of every community plugin and client listed; herdr's site install and plugin counts; whether herdr persists pane environment variables; how herdr behaves on Windows under a crash rather than a clean stop.
