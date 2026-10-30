#!/usr/bin/env node
/**
 * Deterministic stand-in for the `ssh` client the native client runs for an
 * SSH host (`apps/native/src/ssh.rs`). `RUSTLING_TULIP_SSH` points the client
 * at it (via the `fake-ssh.cmd` wrapper on Windows), and
 * `RUSTLING_TULIP_FAKE_SSH_STATE` names the JSON state file it reads on every
 * run:
 *
 *   {
 *     "bridge": "<absolute path of rustling-tulipd(.exe)>",
 *     "env": { "<variable>": "<value>", ... },
 *     "fail": null | { "code": <exit code>, "stderr": "<text>" },
 *     "log": "<absolute path of the argv log>"
 *   }
 *
 * Each run appends `{"pid": <this process>, "argv": [...]}` as one JSON line
 * to `log`, then checks the argv shape: `-T`, `-o BatchMode=yes`, and `--`
 * directly before the host and exactly one remote-command argument. A
 * mismatch writes `fake-ssh: unexpected argv: <argv as JSON>` to stderr and
 * exits 2. With `fail` set it writes that stderr and exits with that code, as
 * a refused login would. Otherwise it ignores the host, splits the remote
 * command on whitespace, requires its first word to be `rustling-tulipd`, and
 * runs `bridge` with the rest as arguments and `env` merged over its own
 * environment, stdio inherited, exiting with the bridge's code.
 */
import { spawn } from "node:child_process";
import { appendFileSync, readFileSync } from "node:fs";

/// Prefix of every line this shim prints on stderr.
const TAG = "fake-ssh:";
const STATE_ENV = "RUSTLING_TULIP_FAKE_SSH_STATE";
/// The program the remote command must run.
const REMOTE_PROGRAM = "rustling-tulipd";

const argv = process.argv.slice(2);
const state = loadState();
logArgv();
if (!shapeOk(argv)) {
  fail(2, `unexpected argv: ${JSON.stringify(argv)}`);
}
if (state.fail) {
  process.stderr.write(`${state.fail.stderr}\n`);
  process.exit(state.fail.code);
}
runBridge(argv[argv.length - 1]);

/**
 * @param {number} code
 * @param {string} message
 * @returns {never}
 */
function fail(code, message) {
  process.stderr.write(`${TAG} ${message}\n`);
  process.exit(code);
}

/** The state file `STATE_ENV` names, checked for the fields every run needs. */
function loadState() {
  const path = process.env[STATE_ENV];
  if (!path) {
    fail(1, `${STATE_ENV} is not set; it names the state file this fake reads`);
  }
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(path, "utf8"));
  } catch (err) {
    fail(1, `reading the state file ${path}: ${err?.message ?? String(err)}`);
  }
  for (const field of ["bridge", "env", "log"]) {
    if (parsed[field] === undefined) {
      fail(1, `the state file ${path} has no ${field}`);
    }
  }
  return parsed;
}

function logArgv() {
  try {
    appendFileSync(state.log, `${JSON.stringify({ pid: process.pid, argv })}\n`);
  } catch (err) {
    fail(1, `appending to the argv log ${state.log}: ${err?.message ?? String(err)}`);
  }
}

/**
 * `-T` and `-o BatchMode=yes` among the options, and the first `--` third
 * from the end: the host, then one remote-command argument.
 *
 * @param {string[]} args
 */
function shapeOk(args) {
  const separator = args.indexOf("--");
  if (separator === -1 || separator !== args.length - 3) {
    return false;
  }
  const options = args.slice(0, separator);
  const batchMode = options.some((arg, i) => arg === "-o" && options[i + 1] === "BatchMode=yes");
  return options.includes("-T") && batchMode;
}

/**
 * Runs the bridge for `remoteCommand` with the state's environment merged
 * over this process's, stdio inherited, and exits with its code.
 *
 * @param {string} remoteCommand
 */
function runBridge(remoteCommand) {
  const [program, ...args] = remoteCommand.split(/\s+/).filter((word) => word !== "");
  if (program !== REMOTE_PROGRAM) {
    fail(2, `unexpected remote command: ${JSON.stringify(remoteCommand)}`);
  }
  const child = spawn(state.bridge, args, {
    env: mergedEnv(state.env),
    stdio: "inherit",
    windowsHide: true,
  });
  child.on("error", (err) => fail(1, `starting ${state.bridge}: ${err?.message ?? String(err)}`));
  child.on("exit", (code, signal) => {
    if (code === null) {
      fail(1, `${state.bridge} ended by signal ${signal}`);
    }
    process.exit(code);
  });
}

/**
 * This process's environment with `overrides` set over it. Windows names
 * are case-insensitive, so an inherited `Path` gives way to an override's
 * `PATH` there.
 *
 * @param {Record<string, string>} overrides
 */
function mergedEnv(overrides) {
  const merged = { ...process.env };
  if (process.platform === "win32") {
    const names = new Set(Object.keys(overrides).map((name) => name.toUpperCase()));
    for (const name of Object.keys(merged)) {
      if (names.has(name.toUpperCase())) {
        delete merged[name];
      }
    }
  }
  return { ...merged, ...overrides };
}
