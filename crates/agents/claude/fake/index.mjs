#!/usr/bin/env node
/**
 * Deterministic stand-in for the `claude` CLI the daemon spawns. The Claude
 * plugin (`crates/agents/claude`) returns this folder's shim from `fake_cli`,
 * and the live test harness points the plugin's `program_env_var()`,
 * `RUSTLING_TULIP_CLAUDE`, at it (the `fake-claude.cmd` wrapper on Windows),
 * which gives the harness a long-running PTY child that doesn't depend on a
 * real install, real credentials or a logged-in account.
 *
 * Behavior:
 *   - Prints a stable banner the smoke test asserts on.
 *   - Paints a bottom region the way Claude's TUI does, redrawn in place under
 *     the lines it prints: the idle prompt box (`❯ ` between two `─` rules
 *     over an auto-mode footer), the `· Cooking…` spinner line over the box
 *     while a turn runs, the `⎿  Interrupted · …` row over it after an Esc,
 *     the `✻ Churned for …` line and a `1 shell` footer after a turn that
 *     leaves a background shell, and an `AskUserQuestion` dialog with its
 *     `Esc to cancel` footer. Every paint sets the title to `◐ <name>` while
 *     working and `✳ <name>` otherwise, the name being the last `/rename`
 *     title or `Claude Code`; a resize repaints the region.
 *   - An Esc that arrives alone, with nothing after it for 50 ms, is the Esc
 *     key: it interrupts a running turn, painting the interrupt row and
 *     running no hook, as Claude fires none on Esc; outside a turn it is
 *     ignored. An Esc with anything after it is ordinary input.
 *   - Echoes the draft typed or pasted into the prompt box as `❯ <text>`
 *     (one row, paste markers and control bytes left out, nothing while an
 *     EOT paste is buffered), so the daemon's chat send sees it there.
 *   - Runs a turn for any submitted line that is not blank or a `/` command:
 *     a `user` transcript record and `UserPromptSubmit`, the spinner, then the
 *     answer, a `system` `turn_duration` record, `Stop` and the idle box. A
 *     message holding "rt-perm" first writes a `Bash` `tool_use` record, runs
 *     `PermissionRequest` and paints the permission dialog; a bare `1`/`2`
 *     there writes the `tool_result`, runs `PostToolUse` and answers, a `3`
 *     denies it and runs `Stop`.
 *   - Writes that transcript to
 *     `<CLAUDE_CONFIG_DIR>/projects/<encoded cwd>/<id>.jsonl`, the id from
 *     `--session-id` or `--resume`; nothing without either.
 *   - Echoes lines from stdin back so input from the harness is observable.
 *   - Reports the exact byte count + sha256 of an EOT-terminated payload as
 *     "RT_PASTE_RESULT bytes=<n> sha=<hex>" for paste byte-fidelity assertions
 *     (EOT survives ConPTY's raw-mode delivery; bracketed-paste markers don't).
 *   - Emits an OSC window-title update for "/rename <title>".
 *   - Emits deterministic streaming output for "/stream".
 *   - Reports one environment variable for "/env <NAME>" as
 *     "RT_ENV <NAME> unset" when unset or empty, "RT_ENV <NAME> sha256=<hex>"
 *     when NAME ends in _KEY or _TOKEN, else "RT_ENV <NAME> <value>".
 *   - Reads the `--settings <path>` hooks document the daemon injects. On
 *     "/rt-hook prompt" it runs the `UserPromptSubmit` hooks with a prompt
 *     event on stdin and paints the spinner, leaving the turn open; on
 *     "/rt-hook ask" it runs the `PreToolUse` hooks with an `AskUserQuestion`
 *     event, paints the question dialog, then runs the `Stop` hooks 5 s later
 *     and paints the idle box; on "/rt-hook bg" it runs the `UserPromptSubmit`
 *     hooks and then a `Stop` whose `background_tasks` lists a running shell,
 *     and paints the background footer. A missing or unreadable settings file
 *     leaves the run hookless rather than failing it.
 *   - Exits cleanly on "/exit\n" or SIGTERM/SIGBREAK.
 *   - Acknowledges (but does not act on) the `--add-dir`, `-p`,
 *     `--model`, and `--permission-mode` flags the daemon may pass.
 */
import { spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

/// Prefix every line this shim prints carries, so its output names the agent
/// it is standing in for.
const TAG = "[fake-claude]";
const READY_BANNER = `${TAG} ready`;
/// How long after "/rt-hook ask" the fake runs its `Stop` hooks: long enough
/// for a spec to observe the prompt, short enough to stay inside its wait.
const STOP_AFTER_MS = 5000;
/// The status row under the prompt box in auto mode.
const AUTO_FOOTER = "  ⏵⏵ auto mode on (shift+tab to cycle)";
/// The substring that makes a submitted message's turn ask a tool permission.
const PERMISSION_CUE = "rt-perm";
/// The pause between the steps of a message's turn, so each step reaches the
/// screen and the transcript on its own.
const TURN_STEP_MS = 300;
/// The tool call the permission cue asks to run.
const FAKE_TOOL = { command: "echo rt-fake", description: "Print the fake's marker" };
/// What the turn answers once the permitted tool call has run.
const TOOL_ANSWER = "Ran echo rt-fake and it printed rt-fake.";
/// Bracketed-paste markers, kept out of a draft and a submitted line when
/// they reach this process.
const PASTE_MARKERS = /\x1b\[20[01]~/g;
/// The states whose region holds the prompt box a draft is echoed into.
const PROMPT_BOX_STATES = new Set(["idle", "working", "interrupted", "background"]);

// Terminator for a paste-fidelity payload. A real bracketed paste's
// \x1b[200~/\x1b[201~ markers do NOT survive ConPTY's raw-mode input delivery
// to a non-native (Node) child — ConPTY interprets and strips them — but the
// payload bytes and a plain control byte like EOT pass through intact. So the
// paste e2e frames its payload with a trailing EOT and we checksum everything
// received up to it, which verifies byte-exact delivery of the payload content
// (where a dropped "middle" would show up) end-to-end.
const PASTE_EOT = "\x04";

/// Input received and not yet handled: the draft the prompt box echoes.
let buffer = "";
/// The draft painted last in the prompt box.
let paintedDraft = "";
/// The uuid of the transcript record written last, the next one's parent.
let lastRecordUuid = null;
/// When the running message turn started.
let turnStartedAt = 0;
/// The id of the tool call the permission dialog on screen asks about.
let pendingToolId = null;

/// The row strings of the bottom region painted last; empty while none is on
/// screen.
let paintedRegion = [];
/// The terminal rows `paintedRegion` takes up, wrapped rows included.
let paintedRows = 0;
/// How long a lone Esc waits for the rest of a key sequence before it counts
/// as the Esc key.
const ESC_HOLD_MS = 50;
/// The timer that turns a held-back Esc into an interrupt.
let escTimer = null;
/// The state painted last, which an Esc and a resize read.
let paintedState = "idle";
/// The title name the state glyph precedes: the last `/rename` title.
let titleName = "Claude Code";

// A crash here used to be invisible: node exits 1, the PTY closes, and the
// daemon just reports "child exited code=1" with no cause — which reads as a
// product bug and cost a long bisect to trace back here. Record it somewhere
// that survives the PTY teardown, and on the PTY itself for good measure.
const CRASH_LOG = join(tmpdir(), "fake-claude-crash.log");
function reportFatal(kind, err) {
  const detail = `${new Date().toISOString()} ${kind}: ${err?.stack ?? String(err)}\n`;
  try {
    appendFileSync(CRASH_LOG, detail);
  } catch {
    /* nothing more we can do */
  }
  try {
    process.stderr.write(`${TAG} ${kind}: ${err?.stack ?? String(err)}\r\n`);
  } catch {
    /* stderr may already be gone */
  }
}
process.on("uncaughtException", (err) => {
  reportFatal("uncaughtException", err);
  process.exit(1);
});
process.on("unhandledRejection", (err) => {
  reportFatal("unhandledRejection", err);
  process.exit(1);
});

const args = process.argv.slice(2);
const flags = parseArgs(args);
// Every interactive Claude spawn passes `--settings`; a run whose file is
// missing or malformed stays hookless with a line naming the path, because the
// daemon itself spawns without hooks when it cannot write the file.
const settingsDoc = loadSettings(flags.settings);
// The transcript a message turn writes, where the daemon's chat mirror reads
// it; null (nothing written) without a conversation id or a Claude home.
const transcriptFile = transcriptPath(flags.conversationId);

process.stdout.write(`${READY_BANNER} (pid: ${process.pid})\r\n`);
if (flags.addDirs.length > 0) {
  process.stdout.write(
    `${TAG} add-dir: ${flags.addDirs.join(", ")}\r\n`,
  );
}
if (flags.model) {
  process.stdout.write(`${TAG} model: ${flags.model}\r\n`);
}
if (flags.permissionMode) {
  process.stdout.write(`${TAG} permission-mode: ${flags.permissionMode}\r\n`);
}
if (flags.skipPermissions) {
  process.stdout.write(`${TAG} dangerously-skip-permissions: yes\r\n`);
}
if (flags.prompt !== null) {
  process.stdout.write(`${TAG} prompt: ${flags.prompt}\r\n`);
}
paint("idle");
process.stdout.on("resize", () => {
  // The terminal has already reflowed the region at the new width.
  paintedRows = terminalRows(paintedRegion, terminalWidth());
  paint(paintedState);
});

// Behave like a real agent TUI: raw mode so input bytes arrive immediately
// rather than being held (and length-capped) by ConPTY's cooked line
// discipline. A large payload with no trailing newline would otherwise never
// be delivered — the canonical buffer waits for an Enter that never comes.
// Guard on isTTY so piped-stdin contexts (unit harnesses) still work.
if (process.stdin.isTTY && process.stdin.setRawMode) {
  process.stdin.setRawMode(true);
}
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  if (escTimer !== null) {
    clearTimeout(escTimer);
    escTimer = null;
  }
  buffer += chunk;
  pump();
  echoDraft();
  // An Esc left alone may be the start of a key sequence split across
  // chunks; it is the Esc key only if nothing follows it in time.
  if (buffer === "\x1b") {
    escTimer = setTimeout(() => {
      escTimer = null;
      if (buffer === "\x1b") {
        buffer = "";
        interrupt();
      }
    }, ESC_HOLD_MS);
  }
});
process.stdin.on("end", () => process.exit(0));

/**
 * Drain `buffer`: complete lines go to `handleLine` (slash commands + echo); a
 * payload terminated by EOT is checksummed and reported as RT_PASTE_RESULT so
 * the paste e2e can assert byte-for-byte fidelity of what actually arrived,
 * rather than spot-checking the first and last bytes.
 */
function pump() {
  for (;;) {
    // A digit while the permission dialog is up answers it, with no Enter.
    if (paintedState === "permission" && /^[123]/.test(buffer)) {
      const digit = buffer[0];
      buffer = buffer.slice(1);
      answerPermission(digit);
      continue;
    }

    const eot = buffer.indexOf(PASTE_EOT);
    const lineBreak = findLineBreak(buffer);

    // An EOT before the next line break closes a paste payload.
    if (eot !== -1 && (lineBreak === -1 || eot < lineBreak)) {
      const payload = buffer.slice(0, eot);
      buffer = buffer.slice(eot + PASTE_EOT.length);
      emitPasteResult(payload);
      continue;
    }

    if (lineBreak !== -1) {
      const line = buffer.slice(0, lineBreak);
      buffer = buffer.slice(lineBreak + lineBreakLength(buffer, lineBreak));
      handleLine(line.replace(PASTE_MARKERS, ""));
      continue;
    }

    // No complete line and no EOT — wait for more input.
    return;
  }
}

/**
 * @param {string} text
 */
function emitPasteResult(text) {
  const bytes = Buffer.byteLength(text, "utf8");
  const sha = createHash("sha256").update(text, "utf8").digest("hex");
  paint("idle", [`${TAG} RT_PASTE_RESULT bytes=${bytes} sha=${sha}`]);
}

/**
 * The text typed so far and not yet submitted, as the prompt box shows it:
 * paste markers and control bytes dropped, a backspace taking the character
 * before it. Empty while an EOT-terminated payload is buffered, so a
 * paste-fidelity payload is never echoed.
 *
 * @returns {string}
 */
function draftText() {
  if (buffer.includes(PASTE_EOT)) {
    return "";
  }
  const chars = [];
  for (const char of buffer.replace(PASTE_MARKERS, "")) {
    if (char === "\x7f" || char === "\b") {
      chars.pop();
    } else if (char >= " ") {
      chars.push(char);
    }
  }
  return chars.join("");
}

/**
 * Repaints the prompt box when the draft in it changed, the way Claude echoes
 * what is typed or pasted into its box.
 */
function echoDraft() {
  if (PROMPT_BOX_STATES.has(paintedState) && draftText() !== paintedDraft) {
    paint(paintedState);
  }
}

/**
 * An Esc: a running turn ends on the interrupt row, with no hook, as Claude
 * fires none on Esc; any other state ignores it.
 */
function interrupt() {
  if (paintedState === "working") {
    paint("interrupted", [`${TAG} interrupted`]);
  }
}

/**
 * The rows of the bottom region `state` paints, sized to the terminal.
 *
 * @param {string} state
 * @returns {string[]}
 */
function regionFor(state) {
  const cols = Math.max(20, terminalWidth() - 2);
  const rule = "─".repeat(cols);
  // One row only: a longer draft is cut on screen, as a wrapped row would
  // shift the region's rows.
  const draft = [...draftText()].slice(0, cols - 2).join("");
  const promptBox = [rule, `❯ ${draft}`, rule];
  switch (state) {
    case "idle":
      return [...promptBox, AUTO_FOOTER];
    case "working":
      return ["· Cooking… (21s · ↓ 1.1k tokens)", "", ...promptBox, AUTO_FOOTER];
    case "interrupted":
      return [
        "  ⎿  Interrupted · What should Claude do instead?",
        "",
        ...promptBox,
        AUTO_FOOTER,
      ];
    case "background":
      return [
        "✻ Churned for 6s · done 8:01 PM",
        "",
        ...promptBox,
        "  ⏵⏵ bypass permissions on · 1 shell · ← for agents · ↓ to manage",
      ];
    case "asking":
      return questionRows(cols);
    case "permission":
      return permissionRows(cols);
    default:
      throw new Error(`no region for state ${state}`);
  }
}

/**
 * The `AskUserQuestion` dialog the "/rt-hook ask" cue asks.
 *
 * @param {number} cols
 * @returns {string[]}
 */
function questionRows(cols) {
  const rule = `  ${"─".repeat(cols - 2)}`;
  return [
    rule,
    "  ──",
    "  ←  ☐ Drink  ✔ Submit  →",
    "",
    "  Do you prefer tea or coffee?",
    "",
    "  ❯ 1. Tea",
    "    2. Coffee",
    "    3. Type something.",
    rule,
    "  ──",
    "    4. Chat about this",
    "",
    "  Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
  ];
}

/**
 * The permission dialog the `rt-perm` cue asks, in place of the prompt box:
 * one rule, the tool and its command, and the three numbered answers over the
 * `Esc to cancel · Tab to amend` footer.
 *
 * @param {number} cols
 * @returns {string[]}
 */
function permissionRows(cols) {
  return [
    "─".repeat(cols),
    " Bash command",
    "",
    `   ${FAKE_TOOL.command}`,
    `   ${FAKE_TOOL.description}`,
    "",
    " Do you want to proceed?",
    " ❯ 1. Yes",
    `   2. Yes, and don't ask again for: ${FAKE_TOOL.command} *`,
    "   3. No",
    "",
    " Esc to cancel · Tab to amend",
  ];
}

/**
 * Redraw the bottom region as `state`, with `logLines` printed above it:
 * the previous region is cleared from its first row down, the lines join the
 * log, and the new region follows with the cursor left on its last row. The
 * title is set to the state's glyph and the session's name.
 *
 * @param {string} state
 * @param {string[]} [logLines]
 */
function paint(state, logLines = []) {
  const rows = regionFor(state);
  const glyph = state === "working" ? "◐" : "✳";
  let out = clearRegion();
  for (const line of logLines) {
    out += `${line}\r\n`;
  }
  out += `\x1b]0;${glyph} ${titleName}\x07`;
  out += rows.join("\r\n");
  process.stdout.write(out);
  paintedRegion = rows;
  paintedRows = terminalRows(rows, terminalWidth());
  paintedState = state;
  paintedDraft = draftText();
}

/**
 * The sequence that clears the painted region, leaving the cursor at the
 * start of its first row, so what follows lands in the log rather than over
 * the region; empty when no region is on screen. Forgets the region.
 *
 * @returns {string}
 */
function clearRegion() {
  if (paintedRows === 0) {
    return "";
  }
  const up = paintedRows > 1 ? `\x1b[${paintedRows - 1}A` : "";
  paintedRegion = [];
  paintedRows = 0;
  return `\r${up}\x1b[J`;
}

/**
 * Print `line` to the log without repainting the region after it.
 *
 * @param {string} line
 */
function writeLog(line) {
  process.stdout.write(`${clearRegion()}${line}\r\n`);
}

/** The terminal's width in columns. */
function terminalWidth() {
  return process.stdout.columns || 120;
}

/**
 * The terminal rows `rows` take up at `width` columns, a row wider than the
 * terminal wrapping onto more.
 *
 * @param {string[]} rows
 * @param {number} width
 * @returns {number}
 */
function terminalRows(rows, width) {
  let total = 0;
  for (const row of rows) {
    total += Math.max(1, Math.ceil([...row].length / width));
  }
  return total;
}

const exitClean = () => process.exit(0);
process.on("SIGTERM", exitClean);
process.on("SIGINT", exitClean);
process.on("SIGBREAK", exitClean);

/**
 * @param {string} line
 */
function handleLine(line) {
  if (line === "/exit") {
    writeLog(`${TAG} bye`);
    process.exit(0);
  }
  if (line.startsWith("/rename ")) {
    const title = sanitizeOscTitle(line.slice("/rename ".length).trim());
    if (title.length > 0) {
      process.stdout.write(`\x1b]0;${title}\x07`);
      titleName = title;
      paint("idle", [`${TAG} renamed: ${title}`]);
    } else {
      paint("idle", [`${TAG} rename ignored: empty title`]);
    }
    return;
  }
  if (line === "/stream") {
    emitStreamOutput();
    return;
  }
  if (line.startsWith("/env ")) {
    emitEnv(line.slice("/env ".length).trim());
    return;
  }
  if (line === "/rt-hook prompt") {
    runHooks("UserPromptSubmit", {
      hook_event_name: "UserPromptSubmit",
      prompt: line,
      permission_mode: "auto",
    });
    paint("working", [`${TAG} working`]);
    return;
  }
  if (line === "/rt-hook bg") {
    runHooks("UserPromptSubmit", {
      hook_event_name: "UserPromptSubmit",
      prompt: line,
      permission_mode: "auto",
    });
    runHooks("Stop", {
      hook_event_name: "Stop",
      last_assistant_message: "started",
      background_tasks: [
        {
          id: "bfake0001",
          type: "shell",
          status: "running",
          description: "fake background shell",
          command: "sleep 600",
        },
      ],
      session_crons: [],
    });
    paint("background", [`${TAG} background`]);
    return;
  }
  if (line === "/rt-hook ask") {
    runHooks(
      "PreToolUse",
      {
        hook_event_name: "PreToolUse",
        tool_name: "AskUserQuestion",
        tool_input: {
          questions: [
            {
              question: "Do you prefer tea or coffee?",
              header: "Drink",
              options: [{ label: "Tea" }, { label: "Coffee" }],
              multiSelect: false,
            },
          ],
        },
      },
      "AskUserQuestion",
    );
    paint("asking");
    setTimeout(() => {
      runHooks("Stop", {
        hook_event_name: "Stop",
        last_assistant_message: "Which do you prefer?",
      });
      paint("idle");
    }, STOP_AFTER_MS);
    return;
  }
  if (line.trim() === "" || line.startsWith("/")) {
    paint("idle", [`${TAG} echo: ${line}`]);
    return;
  }
  startTurn(line);
}

/**
 * A submitted message's turn: the user record and `UserPromptSubmit`, the
 * spinner, then either the permission dialog (a message holding the cue) or
 * the answer and the turn's end.
 *
 * @param {string} text
 */
function startTurn(text) {
  turnStartedAt = Date.now();
  appendRecord({
    type: "user",
    promptId: randomUUID(),
    message: { role: "user", content: text },
  });
  runHooks("UserPromptSubmit", {
    hook_event_name: "UserPromptSubmit",
    prompt: text,
    permission_mode: "auto",
  });
  paint("working", [`${TAG} echo: ${text}`]);
  const next = text.includes(PERMISSION_CUE)
    ? askPermission
    : () => endTurn(`Done: ${text}`);
  setTimeout(next, TURN_STEP_MS);
}

/**
 * The turn's tool call asks its permission: the `tool_use` record, the
 * `PermissionRequest` hooks, and the dialog that waits for a digit.
 */
function askPermission() {
  pendingToolId = `toolu_${randomUUID().replaceAll("-", "")}`;
  appendRecord({
    type: "assistant",
    message: {
      role: "assistant",
      content: [
        { type: "tool_use", id: pendingToolId, name: "Bash", input: FAKE_TOOL },
      ],
    },
  });
  runHooks(
    "PermissionRequest",
    {
      hook_event_name: "PermissionRequest",
      tool_name: "Bash",
      tool_input: FAKE_TOOL,
    },
    "Bash",
  );
  paint("permission");
}

/**
 * The digit typed at the permission dialog: `1` or `2` runs the tool and
 * answers, `3` denies it and ends the turn as Claude does on a refusal.
 *
 * @param {string} digit
 */
function answerPermission(digit) {
  const allowed = digit !== "3";
  appendRecord({
    type: "user",
    message: {
      role: "user",
      content: [
        {
          type: "tool_result",
          tool_use_id: pendingToolId,
          content: allowed ? "rt-fake" : "The user doesn't want to proceed with this tool use.",
          is_error: !allowed,
        },
      ],
    },
  });
  pendingToolId = null;
  if (!allowed) {
    appendRecord({
      type: "user",
      message: {
        role: "user",
        content: [{ type: "text", text: "[Request interrupted by user for tool use]" }],
      },
    });
    runHooks("Stop", { hook_event_name: "Stop", last_assistant_message: "" });
    paint("idle", [`${TAG} denied: ${FAKE_TOOL.command}`]);
    return;
  }
  runHooks(
    "PostToolUse",
    {
      hook_event_name: "PostToolUse",
      tool_name: "Bash",
      tool_input: FAKE_TOOL,
      tool_response: { stdout: "rt-fake", stderr: "", interrupted: false },
    },
    "Bash",
  );
  paint("working", [`${TAG} ran: ${FAKE_TOOL.command}`]);
  setTimeout(() => endTurn(TOOL_ANSWER), TURN_STEP_MS);
}

/**
 * The turn's end: the answer and `turn_duration` records, the `Stop` hooks,
 * and the idle box under the answer.
 *
 * @param {string} answer
 */
function endTurn(answer) {
  appendRecord({
    type: "assistant",
    message: { role: "assistant", content: [{ type: "text", text: answer }] },
  });
  appendRecord({
    type: "system",
    subtype: "turn_duration",
    durationMs: Date.now() - turnStartedAt,
  });
  runHooks("Stop", { hook_event_name: "Stop", last_assistant_message: answer });
  paint("idle", [`${TAG} answer: ${answer}`]);
}

/**
 * Where the transcript of conversation `id` lives: under the Claude home the
 * daemon gives the run (`CLAUDE_CONFIG_DIR`), in the folder named after the
 * working directory, which the daemon started the session in. Null without
 * an id or a home.
 *
 * @param {string | null} id
 * @returns {string | null}
 */
function transcriptPath(id) {
  const home = process.env.CLAUDE_CONFIG_DIR;
  if (id === null || home === undefined || home === "") {
    return null;
  }
  return join(home, "projects", encodeProjectDir(process.cwd()), `${id}.jsonl`);
}

/**
 * The folder name Claude gives a working directory: every character outside
 * `[A-Za-z0-9-]` becomes `-`, one per code point, case kept.
 *
 * @param {string} cwd
 * @returns {string}
 */
function encodeProjectDir(cwd) {
  return cwd.replace(/[^A-Za-z0-9-]/gu, "-");
}

/**
 * Appends one transcript record, chained to the one before it, carrying the
 * conversation id, the working directory and the time; writes nothing when
 * the run has no transcript.
 *
 * @param {object} fields
 */
function appendRecord(fields) {
  if (transcriptFile === null) {
    return;
  }
  const uuid = randomUUID();
  const record = {
    parentUuid: lastRecordUuid,
    uuid,
    sessionId: flags.conversationId,
    cwd: process.cwd(),
    timestamp: new Date().toISOString(),
    ...fields,
  };
  mkdirSync(dirname(transcriptFile), { recursive: true });
  appendFileSync(transcriptFile, `${JSON.stringify(record)}\n`);
  lastRecordUuid = uuid;
}

/**
 * Runs every hook the settings document configures for `event`, with
 * `payload` on its stdin, before this returns. Each one is synchronous on
 * purpose: the daemon applies the event before it answers the hook, so the
 * cue that follows already sees the status the daemon holds, and the hooks'
 * ordering stamps stay increasing. The document's `async` field is ignored.
 *
 * @param {string} event
 * @param {object} payload
 * @param {string} [toolName] the matcher's subject, the event's tool name
 */
function runHooks(event, payload, toolName) {
  const groups = settingsDoc?.hooks?.[event];
  if (!Array.isArray(groups)) {
    return;
  }
  for (const group of groups) {
    if (!hookGroupMatches(group, toolName)) {
      continue;
    }
    const hooks = Array.isArray(group?.hooks) ? group.hooks : [];
    for (const hook of hooks) {
      if (hook?.type !== "command" || typeof hook.command !== "string") {
        continue;
      }
      const result = spawnSync(
        hook.command,
        Array.isArray(hook.args) ? hook.args : [],
        {
          input: JSON.stringify(payload),
          timeout: 5000,
          windowsHide: true,
        },
      );
      // A hook that cannot start would otherwise fail the spec only when its
      // status never arrives, with nothing naming the cause.
      if (result.error !== undefined || result.status === null) {
        const reason = result.error?.message ?? "no exit status";
        writeLog(`${TAG} hook-error ${event}: ${reason}`);
      }
    }
  }
}

/**
 * Whether one entry of an event's hook list applies to the event. An entry
 * with no matcher runs for every event; one with a matcher runs only when the
 * whole tool name matches it, as the agent CLI matches matchers.
 *
 * @param {any} group
 * @param {string | undefined} toolName
 * @returns {boolean}
 */
function hookGroupMatches(group, toolName) {
  if (typeof group?.matcher !== "string") {
    return true;
  }
  if (typeof toolName !== "string") {
    return false;
  }
  try {
    return new RegExp(`^(?:${group.matcher})$`).test(toolName);
  } catch {
    return false;
  }
}

/**
 * Report what this process sees in environment variable `name`, so a spec can
 * check a spawn's env reached the child. A credential-shaped name (ending in
 * `_KEY` or `_TOKEN`) is reported as the SHA-256 of its value, never the value,
 * so the value never lands in the session's scrollback.
 *
 * @param {string} name
 */
function emitEnv(name) {
  const value = process.env[name];
  let report;
  if (value === undefined || value === "") {
    report = "unset";
  } else if (name.endsWith("_KEY") || name.endsWith("_TOKEN")) {
    report = `sha256=${createHash("sha256").update(value, "utf8").digest("hex")}`;
  } else {
    report = value;
  }
  paint("idle", [`${TAG} RT_ENV ${name} ${report}`]);
}

/**
 * Twenty long chunk lines 80 ms apart, written into the log with no repaint
 * between them, then the idle box.
 */
function emitStreamOutput() {
  let chunk = 0;
  const interval = setInterval(() => {
    chunk += 1;
    writeLog(
      `${TAG} stream ${chunk.toString().padStart(2, "0")} ${"x".repeat(360)}`,
    );
    if (chunk >= 20) {
      clearInterval(interval);
      paint("idle");
    }
  }, 80);
}

/**
 * @param {string} value
 */
function sanitizeOscTitle(value) {
  let sanitized = "";
  for (const char of value) {
    const codePoint = char.codePointAt(0);
    if (codePoint !== undefined && !isUnsafeTitleCodePoint(codePoint)) {
      sanitized += char;
    }
  }
  return sanitized.replace(/\s+/g, " ").trim().slice(0, 240);
}

/**
 * @param {number} codePoint
 */
function isUnsafeTitleCodePoint(codePoint) {
  return (
    codePoint < 0x20 ||
    (codePoint >= 0x7f && codePoint <= 0x9f) ||
    codePoint === 0x061c ||
    codePoint === 0x200e ||
    codePoint === 0x200f ||
    (codePoint >= 0x202a && codePoint <= 0x202e) ||
    (codePoint >= 0x2066 && codePoint <= 0x2069)
  );
}

/**
 * @param {string} value
 */
function findLineBreak(value) {
  for (let i = 0; i < value.length; i++) {
    const char = value[i];
    if (char === "\r" || char === "\n") {
      return i;
    }
  }
  return -1;
}

/**
 * @param {string} value
 * @param {number} index
 */
function lineBreakLength(value, index) {
  return value[index] === "\r" && value[index + 1] === "\n" ? 2 : 1;
}

/**
 * @param {string[]} argv
 */
function parseArgs(argv) {
  /** @type {{addDirs: string[], prompt: string | null, model: string | null, permissionMode: string | null, skipPermissions: boolean, settings: string | null, conversationId: string | null}} */
  const out = {
    addDirs: [],
    prompt: null,
    model: null,
    permissionMode: null,
    skipPermissions: false,
    settings: null,
    conversationId: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--add-dir" && argv[i + 1] !== undefined) {
      out.addDirs.push(argv[++i]);
    } else if (a === "-p" && argv[i + 1] !== undefined) {
      out.prompt = argv[++i];
    } else if (a === "--model" && argv[i + 1] !== undefined) {
      out.model = argv[++i];
    } else if (a === "--permission-mode" && argv[i + 1] !== undefined) {
      out.permissionMode = argv[++i];
    } else if (a === "--settings" && argv[i + 1] !== undefined) {
      out.settings = argv[++i];
    } else if ((a === "--session-id" || a === "--resume") && argv[i + 1] !== undefined) {
      out.conversationId = argv[++i];
    } else if (a === "--dangerously-skip-permissions") {
      out.skipPermissions = true;
    }
  }
  return out;
}

/**
 * The hooks document the spawn's `--settings` path names, or null when the
 * spawn named none or the file cannot be read or parsed. Either failure
 * prints one line and leaves the run hookless rather than failing it: the
 * daemon spawns without hooks when it cannot write the file, and every
 * interactive spawn on this branch passes the flag.
 *
 * @param {string | null} path
 * @returns {any | null}
 */
function loadSettings(path) {
  if (path === null) {
    return null;
  }
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    process.stdout.write(`${TAG} settings-missing ${path}\r\n`);
    return null;
  }
  try {
    return JSON.parse(text);
  } catch {
    process.stdout.write(`${TAG} settings-invalid ${path}\r\n`);
    return null;
  }
}
