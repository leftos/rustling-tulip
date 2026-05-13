import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type {
  DaemonMessage,
  PresetLaunchJobSnapshot,
  RepoEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("preset launch progress", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  const createdSessionIds = new Set<string>();

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });
    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-preset-progress-"));
    await writeFile(join(fixtureRepo, "README.md"), "preset progress fixture\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);

    const presetDir = join(fixtureRepo, ".rustling-tulip");
    await mkdir(presetDir, { recursive: true });
    await writeFile(
      join(presetDir, "presets.json"),
      JSON.stringify(
        [
          {
            id: "slow-inline",
            name: "Slow inline",
            prompt_sources: [{ kind: "inline" }],
            prompt_template: "{prompt}",
            context_footer_lines: [],
            variables: [],
            branch_template: "main-{index}",
            session_label_template: "progress {index}",
            default_use_worktree: false,
            dangerously_skip_permissions: false,
            model: null,
            permission_mode: null,
            tab_grouping: { kind: "none" },
            injector: { startup_delay_ms: 0, pre_input: [], post_input: [] },
            stagger_ms: 60_000,
          },
        ],
        null,
        2,
      ) + "\n",
    );

    runGit(fixtureRepo, ["add", "."]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
    await mkdir(join(repoRoot, ".tmp", "e2e"), { recursive: true });
  });

  after(async function () {
    if (ws) {
      for (const sessionId of createdSessionIds) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(250);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup: [],
          });
          await delay(100);
        } catch {
          /* best-effort */
        }
      }
      if (registeredRepoId) {
        try {
          ws.send({ type: "remove_repo", repo_id: registeredRepoId });
          await delay(200);
        } catch {
          /* best-effort */
        }
      }
      await ws.close();
    }
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
  });

  it("keeps the dialog open with cancellable progress after launch", async function () {
    if (!ws || !fixtureRepo) {
      throw new Error("setup failed");
    }

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-preset-progress",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r) => r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) {
      throw new Error("fixture repo never registered");
    }
    registeredRepoId = fixture.id;

    const repoRow = await browser.$(
      `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
    );
    await repoRow.waitForExist({ timeout: 10_000 });
    await repoRow.click({ button: "right" });

    const presetButton = await browser.$(
      '//button[normalize-space()="Launch preset · Slow inline"]',
    );
    await presetButton.waitForExist({ timeout: 10_000 });
    await presetButton.click();

    const dialog = await browser.$('[data-testid="preset-launch-dialog"]');
    await dialog.waitForExist({ timeout: 5_000 });
    await browser.$('[data-testid="preset-source-inline-text"]').setValue(
      "- first progress prompt\n- second progress prompt",
    );
    await browser.$('[data-testid="preset-launch-next"]').click();
    await browser.$('[data-testid="preset-preview-row"]').waitForExist({
      timeout: 5_000,
    });

    const firstJobUpdate = ws.waitFor(isPresetJobUpdated, { timeoutMs: 10_000 });
    await browser.$('[data-testid="preset-launch-submit"]').click();
    const firstJob = (await firstJobUpdate).job;
    rememberSessions(firstJob);

    const progress = await browser.$('[data-testid="preset-launch-progress"]');
    await progress.waitForExist({ timeout: 5_000 });
    const status = await browser.$('[data-testid="preset-launch-progress-status"]');
    await browser.waitUntil(
      async () => (await status.getText()) !== "Starting launch…",
      { timeout: 10_000, timeoutMsg: "launch progress never updated" },
    );
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "preset-launch-progress.png"),
    );

    await browser.$('[data-testid="preset-launch-cancel-job"]').click();
    const cancelled = await ws.waitFor(
      (msg): msg is DaemonMessage & {
        type: "preset_launch_job_updated";
        job: PresetLaunchJobSnapshot;
      } =>
        isPresetJobUpdated(msg) &&
        msg.job.job_id === firstJob.job_id &&
        msg.job.status === "cancelled",
      { timeoutMs: 90_000 },
    );
    rememberSessions(cancelled.job);

    await browser.waitUntil(
      async () => (await status.getText()).includes("Cancelled"),
      { timeout: 5_000, timeoutMsg: "cancelled status did not render" },
    );
    expect(await browser.$('[data-testid="preset-launch-cancel-job"]').isEnabled())
      .to.equal(false);
  });

  function rememberSessions(job: PresetLaunchJobSnapshot) {
    for (const sessionId of job.created_session_ids) {
      createdSessionIds.add(sessionId);
    }
  }
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function isPresetJobUpdated(
  m: DaemonMessage,
): m is DaemonMessage & {
  type: "preset_launch_job_updated";
  job: PresetLaunchJobSnapshot;
} {
  return m.type === "preset_launch_job_updated";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
