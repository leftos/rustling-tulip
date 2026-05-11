/**
 * Daemon smoke spec — the working E2E path today.
 *
 * Spawns the rustling-tulipd binary directly with `RUSTLING_TULIP_CLAUDE`
 * pointing at fake-claude, opens a WS as a normal client, and exercises
 * the user-visible flows the UI would produce: register a temp git repo,
 * spawn an interactive session, observe PTY output containing the
 * fake-claude banner, then stop the session.
 *
 * This bypasses the Tauri WebView entirely. The daemon is documented as
 * the stable boundary in CLAUDE.md ("the daemon is the stable boundary"),
 * so a daemon-level smoke is the right canary for "did our changes break
 * the user-visible behavior end-to-end?". A separate webview spec
 * (`webview.spec.ts`) covers the UI rendering — currently skipped pending
 * tauri-driver Windows support.
 *
 * Run with `pnpm test`. The mocharc-cjs config wires Mocha + tsx so this
 * file runs as TypeScript ESM directly.
 */
import {
  execFileSync,
  spawn,
  type ChildProcess,
} from "node:child_process";
import { mkdtemp, rm, unlink, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { homedir, platform, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { expect } from "chai";

import { handshakeFilePath } from "../../../src/handshake.js";
import { shutdownExistingDaemon } from "../../../src/lifecycle.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";
import { DaemonWsClient } from "../../../src/ws-client.js";

const here = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = resolve(here, "..", "..", "..", "..", "..");

const fakeClaude =
  platform() === "win32"
    ? join(here, "..", "..", "..", "fake-claude", "fake-claude.cmd")
    : join(here, "..", "..", "..", "fake-claude", "fake-claude.sh");

const daemonBin =
  platform() === "win32"
    ? join(repoRoot, "target", "debug", "rustling-tulipd.exe")
    : join(repoRoot, "target", "debug", "rustling-tulipd");

const SPAWN_TIMEOUT = 30_000;
const PTY_OUTPUT_TIMEOUT = 20_000;

interface PtyOutputMsg {
  type: "pty_output";
  session_id: string;
  data_b64: string;
  [k: string]: unknown;
}

interface ScrollbackMsg {
  type: "scrollback";
  session_id: string;
  data_b64: string;
  truncated: boolean;
  [k: string]: unknown;
}

describe("rustling-tulip daemon smoke", function () {
  this.timeout(120_000);

  let daemon: ChildProcess | null = null;
  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    if (!existsSync(daemonBin)) {
      throw new Error(
        `daemon binary missing at ${daemonBin}. Run \`cargo build -p daemon\` from the workspace root.`,
      );
    }
    if (!existsSync(fakeClaude)) {
      throw new Error(`fake-claude shim missing at ${fakeClaude}`);
    }

    // Make sure no existing daemon is holding the handshake file.
    await shutdownExistingDaemon({ timeoutMs: 4_000 });
    await unlink(handshakeFilePath()).catch(() => undefined);

    // Start a fresh daemon with fake-claude wired in.
    daemon = spawn(daemonBin, [], {
      stdio: ["ignore", "ignore", "inherit"],
      env: {
        ...process.env,
        RUSTLING_TULIP_CLAUDE: fakeClaude,
      },
    });
    daemon.on("exit", (code, signal) => {
      // eslint-disable-next-line no-console
      console.error(`[daemon-smoke] daemon exited code=${code} signal=${signal}`);
    });

    // Connect via the same WS path the Tauri app uses.
    ws = await DaemonWsClient.open({ waitTimeoutMs: 15_000 });

    // Init a tiny fixture repo. Done at runtime so we don't ship a
    // ceremonial empty .git/ in the source tree.
    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({
          type: "stop_session",
          session_id: spawnedSessionId,
          cleanup: registeredRepoId
            ? [{ repo_id: registeredRepoId, remove_worktree: false }]
            : [],
        });
        await delay(300);
      } catch {
        /* best-effort */
      }
    }
    if (ws && registeredRepoId) {
      try {
        ws.send({ type: "remove_repo", repo_id: registeredRepoId });
        await delay(200);
      } catch {
        /* best-effort */
      }
    }
    if (ws) {
      try {
        ws.send({ type: "shutdown" });
      } catch {
        /* daemon may already be torn down */
      }
      await ws.close();
    }
    if (daemon && daemon.exitCode === null) {
      daemon.kill();
      // Give it a beat to clean up its handshake file.
      await delay(300);
    }
    // Belt-and-suspenders: scrub the handshake file so the user's next app
    // launch starts with a fresh daemon supervisor pass.
    await unlink(handshakeFilePath()).catch(() => undefined);
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
  });

  it("registers a repo, spawns a session, and emits fake-claude output", async function () {
    expect(ws, "ws").to.not.be.null;
    expect(fixtureRepo, "fixtureRepo").to.not.be.null;
    if (!ws || !fixtureRepo) throw new Error("setup failed");
    const fixturePath = fixtureRepo;

    // Register the fixture repo and wait for the broadcast `repos` snapshot.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixturePath, name: "rt-e2e-fixture" });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r) => normalizePath(r.path) === normalizePath(fixturePath),
    );
    expect(fixture, "fixture repo registered").to.exist;
    registeredRepoId = fixture!.id;

    // Collect both live PTY output and the scrollback ring. Whichever
    // contains the banner first wins. We need both because fake-claude
    // emits the banner before we have a session id to subscribe with —
    // those bytes only reach us via scrollback.
    let liveBuffer = "";
    const ptyDone = new Promise<string>((resolveFn, rejectFn) => {
      const timer = setTimeout(() => {
        unsubscribe();
        rejectFn(
          new Error(
            `did not see "[fake-claude] ready" within ${PTY_OUTPUT_TIMEOUT}ms.\n` +
              `Last live buffer: ${JSON.stringify(liveBuffer.slice(0, 500))}`,
          ),
        );
      }, PTY_OUTPUT_TIMEOUT);
      const unsubscribe = ws!.onMessage((msg) => {
        if (
          spawnedSessionId !== null &&
          "session_id" in msg &&
          typeof msg["session_id"] === "string" &&
          msg["session_id"] !== spawnedSessionId
        ) {
          return;
        }
        let chunk: string | null = null;
        if (isPtyOutput(msg)) {
          liveBuffer += Buffer.from(msg.data_b64, "base64").toString("utf8");
          chunk = liveBuffer;
        } else if (isScrollback(msg)) {
          chunk = Buffer.from(msg.data_b64, "base64").toString("utf8") + liveBuffer;
        }
        if (chunk && chunk.includes("[fake-claude] ready")) {
          clearTimeout(timer);
          unsubscribe();
          resolveFn(chunk);
        }
      });
    });

    // Spawn an interactive session against fake-claude. use_worktree=false
    // keeps the daemon from creating a worktree dir that we'd then need to
    // clean up.
    const spawnPromise = ws.waitFor(
      (m): m is DaemonMessage & {
        type: "session_updated";
        session: SessionSnapshot;
      } => m.type === "session_updated",
      { timeoutMs: SPAWN_TIMEOUT },
    );
    ws.send({
      type: "spawn_session",
      label: "smoke",
      target: {
        kind: "single",
        repo_id: registeredRepoId,
        branch_name: "main",
        base_branch: null,
        use_worktree: false,
      },
      mode: "interactive",
      initial_prompt: null,
      dangerously_skip_permissions: false,
      model: null,
      permission_mode: null,
      extra_env: [],
      prompt_injector: null,
    });
    const spawnMsg = await spawnPromise;
    spawnedSessionId = spawnMsg.session.id;
    expect(spawnMsg.session.mode).to.equal("interactive");

    // Attach so the daemon starts forwarding pty_output. Then poll the
    // scrollback ring on a short cadence — fake-claude usually prints its
    // banner before we have a session id to subscribe with, so the bytes
    // only reach us via scrollback.
    ws.send({ type: "attach", session_id: spawnedSessionId });
    const scrollPoller = setInterval(() => {
      ws?.send({ type: "load_scrollback", session_id: spawnedSessionId! });
    }, 500);

    try {
      const banner = await ptyDone;
      expect(banner, "fake-claude banner reached the WS").to.include(
        "[fake-claude] ready",
      );
    } finally {
      clearInterval(scrollPoller);
    }
  });
});

function normalizePath(p: string): string {
  return p.replace(/\\/g, "/").toLowerCase();
}

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function isPtyOutput(m: DaemonMessage): m is PtyOutputMsg {
  return m.type === "pty_output";
}

function isScrollback(m: DaemonMessage): m is ScrollbackMsg {
  return m.type === "scrollback";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

// Silence "unused import" for homedir; kept around in case tests later need
// to resolve user paths.
void homedir;
