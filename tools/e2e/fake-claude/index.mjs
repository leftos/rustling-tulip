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
 *   - Exits cleanly on "/exit\n" or SIGTERM/SIGBREAK.
 *   - Acknowledges (but does not act on) the `--add-dir`, `-p`,
 *     `--model`, and `--permission-mode` flags the daemon may pass.
 */
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

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, nl).replace(/\r$/, "");
    buffer = buffer.slice(nl + 1);
    handleLine(line);
  }
});
process.stdin.on("end", () => process.exit(0));

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
  process.stdout.write(`[fake-claude] echo: ${line}\r\n`);
  process.stdout.write(PROMPT);
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
