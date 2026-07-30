#!/usr/bin/env node
/**
 * Deterministic stand-in for the `claude` CLI. The daemon's
 * `claude_program()` honors `RUSTLING_TULIP_CLAUDE`, so pointing that env var
 * at this script (via the `fake-claude.cmd` wrapper on Windows) gives the
 * harness a long-running PTY child that doesn't depend on a real claude
 * install or on real Anthropic API credentials.
 *
 * Behavior:
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
 */
import { createHash } from "node:crypto";
import { appendFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const READY_BANNER = "[fake-claude] ready";
const PROMPT = "fake-claude> ";

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
    process.stderr.write(`[fake-claude] ${kind}: ${err?.stack ?? String(err)}\r\n`);
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

process.stdout.write(`${READY_BANNER} (pid: ${process.pid})\r\n`);
if (flags.addDirs.length > 0) {
  process.stdout.write(
    `[fake-claude] add-dir: ${flags.addDirs.join(", ")}\r\n`,
  );
}
if (flags.model) {
  process.stdout.write(`[fake-claude] model: ${flags.model}\r\n`);
}
if (flags.permissionMode) {
  process.stdout.write(`[fake-claude] permission-mode: ${flags.permissionMode}\r\n`);
}
if (flags.skipPermissions) {
  process.stdout.write("[fake-claude] dangerously-skip-permissions: yes\r\n");
}
if (flags.prompt !== null) {
  process.stdout.write(`[fake-claude] prompt: ${flags.prompt}\r\n`);
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
    `\r\n[fake-claude] RT_PASTE_RESULT bytes=${bytes} sha=${sha}\r\n`,
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
  if (line === "/exit") {
    process.stdout.write("[fake-claude] bye\r\n");
    process.exit(0);
  }
  if (line.startsWith("/rename ")) {
    const title = sanitizeOscTitle(line.slice("/rename ".length).trim());
    if (title.length > 0) {
      process.stdout.write(`\x1b]0;${title}\x07`);
      process.stdout.write(`[fake-claude] renamed: ${title}\r\n`);
    } else {
      process.stdout.write("[fake-claude] rename ignored: empty title\r\n");
    }
    process.stdout.write(PROMPT);
    return;
  }
  if (line === "/stream") {
    emitStreamOutput();
    return;
  }
  process.stdout.write(`[fake-claude] echo: ${line}\r\n`);
  process.stdout.write(PROMPT);
}

function emitStreamOutput() {
  let chunk = 0;
  const interval = setInterval(() => {
    chunk += 1;
    process.stdout.write(
      `[fake-claude] stream ${chunk.toString().padStart(2, "0")} ${"x".repeat(360)}\r\n`,
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
