import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import {
  dismissLayoutChooser,
  isSessionUpdated,
  spawnSession,
} from "../../../src/session-helpers.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
/** Commits pushed onto origin/main without moving local main. */
const STALE_BY = 3;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

/**
 * Covers the spawn dialog's fork-point affordances against a repo shaped like
 * the bug that motivated them: a local `main` that trails `origin/main`,
 * because every session forks off it, works, and pushes, and nobody ever pulls
 * the primary working tree.
 *
 * Three properties, in increasing order of what they'd cost to get wrong:
 * the base field defaults to the remote ref; a stale explicit base is called
 * out; and an existing worktree prompts instead of being silently reused —
 * with "recreate" actually moving the fork point rather than just relabelling
 * it.
 */
describe("spawn dialog fork points", function () {
  this.timeout(240_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let originRepo: string | null = null;
  let registeredRepoId: string | null = null;
  const spawnedSessionIds: string[] = [];
  const worktreePaths: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-fork-point-"));
    originRepo = `${fixtureRepo}-origin`;
    await mkdir(originRepo, { recursive: true });
    runGit(originRepo, ["init", "--bare", "-b", "main"]);

    await writeFile(join(fixtureRepo, "README.md"), "fork point fixture\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["config", "commit.gpgsign", "false"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
    runGit(fixtureRepo, ["remote", "add", "origin", originRepo]);
    runGit(fixtureRepo, ["push", "-u", "origin", "main"]);

    // Advance origin/main without moving local main: commit on a scratch
    // branch, push it onto main, then throw the scratch branch away.
    runGit(fixtureRepo, ["checkout", "-b", "scratch"]);
    for (let i = 0; i < STALE_BY; i += 1) {
      await writeFile(join(fixtureRepo, `upstream-${i}.txt`), "landed\n");
      runGit(fixtureRepo, ["add", "."]);
      runGit(fixtureRepo, ["commit", "-m", `upstream ${i}`]);
    }
    runGit(fixtureRepo, ["push", "origin", "scratch:main"]);
    runGit(fixtureRepo, ["checkout", "main"]);
    runGit(fixtureRepo, ["branch", "-D", "scratch"]);
    runGit(fixtureRepo, ["fetch", "--prune"]);
    expect(behindCount(fixtureRepo, "main", "origin/main")).to.equal(
      STALE_BY,
      "fixture must start with local main trailing origin/main",
    );

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-fork-point-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo?.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo was not registered");
    registeredRepoId = fixture.id;
  });

  // A failed assertion leaves the modal mounted, and its backdrop swallows
  // every click in the next test — turning one real failure into a cascade of
  // misleading "click intercepted" errors.
  afterEach(async function () {
    const dialog = await browser.$('[data-testid="spawn-dialog"]');
    if (await dialog.isExisting()) {
      await closeSpawnDialog().catch(() => undefined);
    }
  });

  after(async function () {
    if (ws) {
      for (const sessionId of [...spawnedSessionIds].reverse()) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(400);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup: [],
          });
          await delay(300);
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
    for (const path of worktreePaths) {
      if (!fixtureRepo) break;
      try {
        runGit(fixtureRepo, ["worktree", "remove", "--force", path]);
      } catch {
        /* best-effort */
      }
    }
    for (const path of [fixtureRepo, originRepo]) {
      if (!path) continue;
      await rm(path, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("defaults the base branch to the remote-tracking ref", async function () {
    if (!registeredRepoId) throw new Error("setup failed");

    const dialog = await openSpawnDialog();
    await ensureWorktreeMode(dialog);

    const base = await dialog.$('[data-testid="spawn-single-base-branch"]');
    await base.waitForExist({ timeout: 10_000 });
    // The dialog fetches on open, so origin/main may take a beat to appear in
    // the remote list that seeds this field.
    await browser.waitUntil(async () => (await base.getValue()) === "origin/main", {
      timeout: 20_000,
      timeoutMsg: `base branch seeded with "${await base.getValue()}", expected origin/main`,
    });

    // No staleness callout when the base already is the remote ref — that
    // would be "origin/main is 0 behind origin/main".
    const stale = await dialog.$('[data-testid="spawn-single-base-stale"]');
    expect(await stale.isExisting()).to.equal(false);
    await closeSpawnDialog();
  });

  it("reports how far a stale local base trails its remote", async function () {
    if (!registeredRepoId) throw new Error("setup failed");

    const dialog = await openSpawnDialog();
    await ensureWorktreeMode(dialog);
    await setBaseBranch(dialog, "main");

    const stale = await dialog.$('[data-testid="spawn-single-base-stale"]');
    await stale.waitForExist({ timeout: 20_000 });
    const text = await stale.getText();
    expect(text).to.include(`${STALE_BY} commits behind`);
    expect(text).to.include("origin/main");
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "spawn-base-stale.png"),
    );
    await closeSpawnDialog();
  });

  it("dismisses the branch suggestions with Escape, keeping the dialog", async function () {
    if (!registeredRepoId) throw new Error("setup failed");

    const dialog = await openSpawnDialog();
    await ensureWorktreeMode(dialog);
    const branch = await dialog.$('[data-testid="spawn-single-branch"]');
    await branch.waitForExist({ timeout: 10_000 });
    await setFieldValue(branch, "wt/escape-check", "branch field");

    const listbox = await dialog.$('[role="listbox"]');
    await listbox.waitForExist({ timeout: 5_000 });

    await browser.keys("Escape");
    await listbox.waitForExist({ timeout: 5_000, reverse: true });
    // The whole point: Escape belongs to the dropdown here, not the modal.
    // Before this was fixed the key reached the modal's own handler too and
    // took the dialog — and everything typed into it — with it.
    expect(await dialog.isExisting(), "spawn dialog survives Escape").to.equal(
      true,
    );
    expect(await branch.getValue()).to.equal("wt/escape-check");

    // With no dropdown open, Escape falls through to the modal as usual.
    await browser.keys("Escape");
    await dialog.waitForExist({ timeout: 5_000, reverse: true });
  });

  it("prompts instead of silently reusing a leftover worktree", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const leftover = await seedLeftoverWorktree(
      ws,
      registeredRepoId,
      "wt/collision-prompt",
    );
    expect(behindCount(leftover, "HEAD", "origin/main")).to.equal(
      STALE_BY,
      "leftover worktree must be forked from the stale local main",
    );

    const dialog = await openSpawnDialog();
    await ensureWorktreeMode(dialog);
    await setBranchName(dialog, "wt/collision-prompt");

    const collision = await dialog.$('[data-testid="spawn-single-collision"]');
    await collision.waitForExist({ timeout: 20_000 });
    expect(await collision.getText()).to.include(`${STALE_BY} commits behind`);

    // Reuse is the default: the destructive option must never be pre-selected.
    const reuse = await dialog.$(
      '[data-testid="spawn-single-collision-reuse"]',
    );
    const recreate = await dialog.$(
      '[data-testid="spawn-single-collision-recreate"]',
    );
    expect(await reuse.isSelected()).to.equal(true);
    expect(await recreate.isSelected()).to.equal(false);

    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "spawn-worktree-collision.png"),
    );
    await closeSpawnDialog();
  });

  it("moves the fork point when recreate-from-base is chosen", async function () {
    if (!ws || !registeredRepoId || !fixtureRepo) throw new Error("setup failed");

    const branchName = "wt/collision-recreate";
    const leftover = await seedLeftoverWorktree(ws, registeredRepoId, branchName);
    expect(behindCount(leftover, "HEAD", "origin/main")).to.equal(STALE_BY);

    const dialog = await openSpawnDialog();
    await ensureWorktreeMode(dialog);
    await setBranchName(dialog, branchName);
    await setBaseBranch(dialog, "origin/main");

    const recreate = await dialog.$(
      '[data-testid="spawn-single-collision-recreate"]',
    );
    await recreate.waitForExist({ timeout: 20_000 });
    await recreate.click();
    await browser.waitUntil(async () => recreate.isSelected(), {
      timeout: 5_000,
      timeoutMsg: "recreate-from-base did not become selected",
    });

    const spawned = ws.waitFor(
      (msg): msg is DaemonMessage & {
        type: "session_updated";
        session: SessionSnapshot;
      } =>
        isSessionUpdated(msg) &&
        msg.session.members.some((m) => m.branch === branchName),
      { timeoutMs: 30_000 },
    );
    const submit = await dialog.$('[data-testid="spawn-single-submit"]');
    await submit.click();

    const session = (await spawned).session;
    spawnedSessionIds.push(session.id);
    const worktree = session.members[0]?.worktree_path;
    if (!worktree) throw new Error("spawned session carried no worktree path");

    expect(behindCount(worktree, "HEAD", "origin/main")).to.equal(
      0,
      "recreated worktree must sit on origin/main, not the stale local main",
    );
  });

  /**
   * Spawn a worktree session based on the *stale* local main, then stop and
   * discard it without cleanup — leaving exactly what the bug produced: a
   * worktree directory on disk owned by no live session.
   */
  async function seedLeftoverWorktree(
    client: DaemonWsClient,
    repoId: string,
    branchName: string,
  ): Promise<string> {
    const session = await spawnSession(client, {
      label: `fork-point-seed-${branchName}`,
      repoId,
      branchName,
      baseBranch: "main",
      useWorktree: true,
    });
    const worktree = session.members[0]?.worktree_path;
    if (!worktree) throw new Error("seed session carried no worktree path");
    worktreePaths.push(worktree);

    client.send({ type: "stop_session", session_id: session.id, cleanup: [] });
    await delay(600);
    client.send({
      type: "discard_session",
      session_id: session.id,
      cleanup: [],
    });
    await delay(400);
    return worktree;
  }
});

/** Commits `left` trails `right`, via `rev-list --count left..right`. */
function behindCount(cwd: string, left: string, right: string): number {
  const out = execFileSync(
    "git",
    ["rev-list", "--count", `${left}..${right}`],
    { cwd, encoding: "utf8" },
  );
  return Number.parseInt(out.trim(), 10);
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

/** What `browser.$` hands back -- a chainable, not an awaited Element. */
type DialogElement = ReturnType<typeof browser.$>;

async function openSpawnDialog() {
  const spawnButton = await browser.$('[data-testid="sidebar-add-session"]');
  await spawnButton.waitForExist({ timeout: 10_000 });
  await browser.waitUntil(async () => spawnButton.isEnabled(), {
    timeout: 10_000,
    timeoutMsg: "spawn button did not become enabled",
  });
  await spawnButton.click();
  const dialog = await browser.$('[data-testid="spawn-dialog"]');
  await dialog.waitForExist({ timeout: 10_000 });
  return dialog;
}

async function closeSpawnDialog(): Promise<void> {
  const close = await browser.$('[data-testid="spawn-close"]');
  await close.click();
  const dialog = await browser.$('[data-testid="spawn-dialog"]');
  await dialog.waitForExist({ timeout: 5_000, reverse: true });
}

/**
 * The base-branch field and the collision prompt only render for a *new*
 * worktree, so both the worktree checkbox and "new" mode have to be on before
 * a spec can assert against them.
 */
async function ensureWorktreeMode(
  dialog: DialogElement,
): Promise<void> {
  const worktree = await dialog.$('[data-testid="spawn-single-worktree"]');
  await worktree.waitForExist({ timeout: 10_000 });
  if (!(await worktree.isSelected())) await worktree.click();
  await browser.waitUntil(async () => worktree.isSelected(), {
    timeout: 5_000,
    timeoutMsg: "worktree toggle did not turn on",
  });
}

/**
 * Replace the contents of a React-controlled text input.
 *
 * Neither of the obvious approaches works here. `setValue` is `clearValue` +
 * `addValue`, and the clear doesn't reach React's state, so the component
 * re-renders its old value and the new text lands appended
 * (`wt/jolly-koala` + `wt/x` = `wt/jolly-koalawt/x`). Select-all-then-type
 * fails too — the WebView doesn't honour the synthetic Ctrl+A, producing
 * `origin/mainmain`.
 *
 * Writing through the native value setter doesn't work either: it updates the
 * DOM (so a naive read-back passes) but React's onChange never fires, so the
 * component's state keeps the old value and the next re-render puts it back.
 *
 * What does work is real key events — focus, Backspace over the existing
 * value, then type. Focus is set programmatically rather than by clicking,
 * because the branch combobox's dropdown overlays the fields below it and
 * would intercept the click.
 */
async function setFieldValue(
  field: Awaited<DialogElement>,
  value: string,
  label: string,
): Promise<void> {
  const BACKSPACE = "";
  await browser.execute((el) => {
    (el as HTMLElement).focus();
  }, field as unknown as HTMLElement);
  const current = await field.getValue();
  if (current.length > 0) {
    // A string sends its characters in sequence; an array would be read as a
    // chord and press them all at once.
    await browser.keys(BACKSPACE.repeat(current.length));
  }
  await browser.keys(value);
  await browser
    .waitUntil(async () => (await field.getValue()) === value, {
      timeout: 5_000,
    })
    .catch(async () => {
      throw new Error(
        `${label} settled on "${await field.getValue()}", expected "${value}"`,
      );
    });
}

/**
 * Close the branch combobox's suggestion list, which otherwise covers the
 * fields below it. It listens for a `mousedown` outside its wrapper — and a
 * WebDriver click can't deliver one, because the open dropdown intercepts the
 * click before it reaches whatever is underneath.
 */
async function dismissComboboxDropdown(): Promise<void> {
  await browser.execute(() => {
    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
  });
}

async function setBranchName(
  dialog: DialogElement,
  value: string,
): Promise<void> {
  const branch = await dialog.$('[data-testid="spawn-single-branch"]');
  await branch.waitForExist({ timeout: 10_000 });
  await setFieldValue(branch, value, "branch field");
  await dismissComboboxDropdown();
}

async function setBaseBranch(
  dialog: DialogElement,
  value: string,
): Promise<void> {
  const base = await dialog.$('[data-testid="spawn-single-base-branch"]');
  await base.waitForExist({ timeout: 10_000 });
  await setFieldValue(base, value, "base branch field");
}
