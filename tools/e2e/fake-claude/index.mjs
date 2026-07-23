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
 *   - Treats a bracketed paste (\x1b[200~ … \x1b[201~) as one opaque payload
 *     and, on the closing marker, reports its exact byte count + sha256 as
 *     "RT_PASTE_RESULT bytes=<n> sha=<hex>" for byte-fidelity assertions.
 *   - Emits an OSC window-title update for "/rename <title>".
 *   - Emits deterministic streaming output for "/stream".
 *   - Exits cleanly on "/exit\n" or SIGTERM/SIGBREAK.
 *   - Acknowledges (but does not act on) the `--add-dir`, `-p`,
 *     `--model`, and `--permission-mode` flags the daemon may pass.
 */
import { createHash } from "node:crypto";

const READY_BANNER = "[fake-claude] ready";
const PROMPT = "fake-claude> ";

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

const PASTE_START = "\x1b[200~";
const PASTE_END = "\x1b[201~";

let buffer = "";
let inPaste = false;
let pasteBuf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  pump();
});
process.stdin.on("end", () => process.exit(0));

/**
 * Drain `buffer`, alternating between line-mode (existing slash commands and
 * echo) and paste-mode. A real agent TUI treats everything between the
 * bracketed-paste markers as one opaque payload; the shim mirrors that and,
 * on the closing marker, reports the exact byte count + sha256 of the payload
 * it received. That lets the paste e2e assert byte-for-byte fidelity instead
 * of only spot-checking the first and last markers. Marker-split-across-chunks
 * is handled by holding a short tail back until more input arrives.
 */
function pump() {
  for (;;) {
    if (inPaste) {
      const endIdx = buffer.indexOf(PASTE_END);
      if (endIdx === -1) {
        // Closing marker not here yet — accumulate all but a possible split
        // marker tail, then wait for the next chunk.
        const keep = PASTE_END.length - 1;
        if (buffer.length > keep) {
          pasteBuf += buffer.slice(0, buffer.length - keep);
          buffer = buffer.slice(buffer.length - keep);
        }
        return;
      }
      pasteBuf += buffer.slice(0, endIdx);
      buffer = buffer.slice(endIdx + PASTE_END.length);
      inPaste = false;
      emitPasteResult(pasteBuf);
      pasteBuf = "";
      continue;
    }

    const startIdx = buffer.indexOf(PASTE_START);
    const lineBreak = findLineBreak(buffer);

    // Flush any complete line that precedes the next paste start.
    if (lineBreak !== -1 && (startIdx === -1 || lineBreak < startIdx)) {
      const line = buffer.slice(0, lineBreak);
      buffer = buffer.slice(lineBreak + lineBreakLength(buffer, lineBreak));
      handleLine(line);
      continue;
    }

    if (startIdx !== -1) {
      // Enter paste mode; drop everything up to and including the marker.
      buffer = buffer.slice(startIdx + PASTE_START.length);
      inPaste = true;
      continue;
    }

    // No complete line and no paste start — wait for more input. (A partial
    // PASTE_START at the tail stays buffered until the next chunk completes
    // it, since indexOf returned -1.)
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
