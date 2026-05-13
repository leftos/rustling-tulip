import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("exit confirmation with worktree sessions", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSession: SessionSnapshot | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-exit-worktree-"));
    await writeFile(join(fixtureRepo, "README.md"), "exit worktree fixture\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-exit-worktree",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (repo) =>
        repo.path === fixtureRepo || repo.path === fixtureRepo?.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;
  });

  after(async function () {
    if (ws && spawnedSession) {
      try {
        ws.send({
          type: "stop_session",
          session_id: spawnedSession.id,
          cleanup: [],
        });
        await delay(300);
        ws.send({
          type: "discard_session",
          session_id: spawnedSession.id,
          cleanup: [],
        });
        await delay(300);
      } catch {
        /* best-effort cleanup */
      }
      const worktreePath = spawnedSession.members[0]?.worktree_path;
      if (fixtureRepo && worktreePath) {
        try {
          runGit(fixtureRepo, ["worktree", "remove", "--force", worktreePath]);
        } catch {
          /* best-effort cleanup */
        }
        await rm(worktreePath, {
          recursive: true,
          force: true,
          maxRetries: 20,
          retryDelay: 100,
        });
      }
    }
    if (ws && registeredRepoId) {
      try {
        ws.send({ type: "remove_repo", repo_id: registeredRepoId });
        await delay(200);
      } catch {
        /* best-effort cleanup */
      }
    }
    await ws?.close();
    if (fixtureRepo) {
      await rm(fixtureRepo, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("separates session and worktree shutdown choices", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    spawnedSession = await spawnPlainShell(ws, registeredRepoId);

    await browser.execute(() => {
      const scope = globalThis as unknown as {
        dispatchEvent: (event: { type: string }) => boolean;
        Event: new (type: string) => { type: string };
      };
      scope.dispatchEvent(new scope.Event("rt:e2e_open_exit_confirm"));
    });
    const dialog = await browser.$('[data-testid="exit-confirm-dialog"]');
    await dialog.waitForExist({ timeout: 5_000 });

    await expectButtonText(
      dialog,
      "exit-stop-keep-worktrees",
      "Stop sessions, keep worktrees",
    );
    await expectButtonText(
      dialog,
      "exit-stop-remove-worktrees",
      "Stop sessions, remove worktrees",
    );
    await expectButtonText(
      dialog,
      "exit-keep-running",
      "Keep sessions running in background",
    );

    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "exit-confirm-worktrees.png"),
    );

    await (await dialog.$('[data-testid="exit-cancel"]')).click();
    await dialog.waitForExist({ timeout: 5_000, reverse: true });
  });
});

async function spawnPlainShell(
  ws: DaemonWsClient,
  repoId: string,
): Promise<SessionSnapshot> {
  const spawned = ws.waitFor(isExitWorktreeSession, { timeoutMs: 20_000 });
  ws.send({
    type: "spawn_session",
    label: "exit-confirm-worktree-shell",
    target: {
      kind: "single",
      repo_id: repoId,
      branch_name: "rt-e2e-exit-confirm-worktree",
      base_branch: null,
      use_worktree: true,
    },
    mode: "plain_shell",
    initial_prompt: null,
    dangerously_skip_permissions: false,
    agent: "claude",
    model: null,
    permission_mode: null,
    codex_sandbox: null,
    extra_env: [],
    prompt_injector: null,
  });
  return (await spawned).session;
}

async function expectButtonText(
  dialog: ReturnType<typeof browser.$>,
  testId: string,
  expected: string,
): Promise<void> {
  const button = await dialog.$(`[data-testid="${testId}"]`);
  await button.waitForExist({ timeout: 2_000 });
  expect(await button.getText()).to.equal(expected);
}

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos";
}

function isExitWorktreeSession(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } {
  if (msg.type !== "session_updated") {
    return false;
  }
  const session = msg.session as SessionSnapshot;
  return (
    session.label === "exit-confirm-worktree-shell" &&
    session.mode === "plain_shell"
  );
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
