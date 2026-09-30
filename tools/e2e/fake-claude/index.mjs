#!/usr/bin/env node
/**
 * Deterministic stand-in for the agent CLIs the daemon spawns — `claude`,
 * `codex` and `cursor-agent`. Each backend's `program_env_var()` honors
 * `RUSTLING_TULIP_CLAUDE` / `RUSTLING_TULIP_CODEX` /
 * `RUSTLING_TULIP_CURSOR_AGENT`, so pointing all three at this script (via
 * the `fake-claude.cmd` wrapper on Windows) gives the harness a long-running
 * PTY child that doesn't depend on a real install, real credentials or a
 * logged-in account. `FAKE_AGENT` picks the mode, `claude` when unset.
 *
 * Behavior (claude mode):
 *   - Prints a stable banner the smoke test asserts on.
 *   - Echoes lines from stdin back so input from the harness is observable.
 *   - Reports the exact byte count + sha256 of an EOT-terminated payload as
 *     "RT_PASTE_RESULT bytes=<n> sha=<hex>" for paste byte-fidelity assertions
 *     (EOT survives ConPTY's raw-mode delivery; bracketed-paste markers don't).
 *   - Emits an OSC window-title update for "/rename <title>".
 *   - Emits deterministic streaming output for "/stream".
 *   - Exits cleanly on "/exit\n" or SIGTERM/SIGBREAK.
 *   - Acknowledges (but does not act on) the `--add-dir`, `-p`,
 *     `--model`, and `--permission-mode` flags the daemon may pass.
 *
 * Codex mode (`FAKE_AGENT=codex`) adds:
 *   - A rollout file under `$CODEX_HOME` written on the first input line,
 *     named and shaped as the daemon's `codex_rollout` reader expects.
 *   - "RT_RESUMED <id>" for argv `resume … <id>`.
 *
 * Cursor mode (`FAKE_AGENT=cursor`) adds:
 *   - A chat id printed by `create-chat`, which the daemon runs to
 *     pre-create a chat before it spawns a session.
 *   - "RT_RESUMED <id>" for argv `--resume <id>`.
 */
import { createHash } from "node:crypto";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

/// The agent this run stands in for; a spec picks it with an env row.
const AGENT = (process.env.FAKE_AGENT ?? "claude").trim().toLowerCase();
/// Prefix every line this shim prints carries, so a mode's output names the
/// agent it is standing in for.
const TAG = `[fake-${AGENT}]`;
const READY_BANNER = `${TAG} ready`;
const PROMPT = `fake-${AGENT}> `;
/// The thread id the codex mode writes into its rollout, and the chat id the
/// cursor mode's `create-chat` prints. Both are asserted by
/// `apps/native/tests/e2e_recover.rs`; changing one means changing both.
const CODEX_THREAD_ID = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const CURSOR_CHAT_ID = "3f1c2b9e-7a4d-4e8f-9b2a-6c5d4e3f2a1b";

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

// The daemon runs `create-chat` to pre-create a Cursor chat before it spawns
// the session, and reads the id off the first line of stdout.
if (AGENT === "cursor" && args.includes("create-chat")) {
  process.stdout.write(`${CURSOR_CHAT_ID}\n`);
  process.exit(0);
}

const resumed = resumedConversation(AGENT, args);

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
if (resumed !== null) {
  process.stdout.write(`${TAG} RT_RESUMED ${resumed}\r\n`);
}
process.stdout.write(PROMPT);

// Terminator for a paste-fidelity payload. A real bracketed paste's
// \x1b[200~/\x1b[201~ markers do NOT survive ConPTY's raw-mode input delivery
// to a non-native (Node) child — ConPTY interprets and strips them — but the
// payload bytes and a plain control byte like EOT pass through intact. So the
// paste e2e frames its payload with a trailing EOT and we checksum everything
// received up to it, which verifies byte-exact delivery of the payload content
// (where a dropped "middle" would show up) end-to-end.
const PASTE_EOT = "\x04";

let buffer = "";
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
  buffer += chunk;
  pump();
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
      handleLine(line);
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
  process.stdout.write(
    `\r\n${TAG} RT_PASTE_RESULT bytes=${bytes} sha=${sha}\r\n`,
  );
  process.stdout.write(PROMPT);
}

const exitClean = () => process.exit(0);
process.on("SIGTERM", exitClean);
process.on("SIGINT", exitClean);
process.on("SIGBREAK", exitClean);

/**
 * @param {string} line
 */
function handleLine(line) {
  writeCodexRolloutOnce();
  if (line === "/exit") {
    process.stdout.write(`${TAG} bye\r\n`);
    process.exit(0);
  }
  if (line.startsWith("/rename ")) {
    const title = sanitizeOscTitle(line.slice("/rename ".length).trim());
    if (title.length > 0) {
      process.stdout.write(`\x1b]0;${title}\x07`);
      process.stdout.write(`${TAG} renamed: ${title}\r\n`);
    } else {
      process.stdout.write(`${TAG} rename ignored: empty title\r\n`);
    }
    process.stdout.write(PROMPT);
    return;
  }
  if (line === "/stream") {
    emitStreamOutput();
    return;
  }
  process.stdout.write(`${TAG} echo: ${line}\r\n`);
  process.stdout.write(PROMPT);
}

/// Whether this run has already written its thread's rollout.
let rolloutWritten = false;

/**
 * Codex writes a thread's rollout on the first user message, and the daemon
 * finds the thread id in that file's name and in its `session_meta` first
 * line. Only a fresh thread writes one: a resumed thread already has its
 * file, whose name predates this run.
 */
function writeCodexRolloutOnce() {
  if (AGENT !== "codex" || rolloutWritten || resumed !== null) {
    return;
  }
  rolloutWritten = true;
  try {
    const home = process.env.CODEX_HOME || join(homedir(), ".codex");
    const now = new Date();
    const day = [
      String(now.getFullYear()),
      pad2(now.getMonth() + 1),
      pad2(now.getDate()),
    ];
    const dir = join(home, "sessions", ...day);
    mkdirSync(dir, { recursive: true });
    const stamp = `${day.join("-")}T${pad2(now.getHours())}-${pad2(now.getMinutes())}-${pad2(now.getSeconds())}`;
    const cwd = process.cwd();
    const meta = JSON.stringify({
      timestamp: now.toISOString(),
      ordinal: 0,
      type: "session_meta",
      payload: {
        id: CODEX_THREAD_ID,
        session_id: CODEX_THREAD_ID,
        cwd,
        source: "cli",
        runtime_workspace_roots: [cwd],
      },
    });
    writeFileSync(
      join(dir, `rollout-${stamp}-${CODEX_THREAD_ID}.jsonl`),
      `${meta}\n`,
    );
  } catch (err) {
    reportFatal("writeCodexRollout", err);
  }
}

/**
 * The conversation a spawn's argv resumes, else null. Codex resumes with
 * `codex resume [flags] <id>`, the id last; cursor-agent with
 * `--resume <id>` (as does a first Cursor spawn, which opens the chat the
 * daemon pre-created). Claude keeps its own behaviour here.
 *
 * @param {string} agent
 * @param {string[]} argv
 * @returns {string | null}
 */
function resumedConversation(agent, argv) {
  if (agent === "codex" && argv[0] === "resume") {
    return resumableId(argv[argv.length - 1]);
  }
  if (agent === "cursor") {
    const at = argv.indexOf("--resume");
    return at === -1 ? null : resumableId(argv[at + 1]);
  }
  return null;
}

/**
 * @param {string | undefined} id
 * @returns {string | null}
 */
function resumableId(id) {
  return id === undefined || id.startsWith("-") ? null : id;
}

/**
 * @param {number} value
 */
function pad2(value) {
  return String(value).padStart(2, "0");
}

function emitStreamOutput() {
  let chunk = 0;
  const interval = setInterval(() => {
    chunk += 1;
    process.stdout.write(
      `${TAG} stream ${chunk.toString().padStart(2, "0")} ${"x".repeat(360)}\r\n`,
    );
    if (chunk >= 20) {
      clearInterval(interval);
      process.stdout.write(PROMPT);
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
  /** @type {{addDirs: string[], prompt: string | null, model: string | null, permissionMode: string | null, skipPermissions: boolean}} */
  const out = {
    addDirs: [],
    prompt: null,
    model: null,
    permissionMode: null,
    skipPermissions: false,
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
    } else if (a === "--dangerously-skip-permissions") {
      out.skipPermissions = true;
    }
  }
  return out;
}
