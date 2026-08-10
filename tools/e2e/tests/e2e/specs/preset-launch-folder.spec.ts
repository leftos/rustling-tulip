/**
 * Preset launch — folder source preview round-trip.
 *
 * The audit's #1 finding: file/folder presets are unlaunchable from the UI
 * because the client-side `computePreview` returns `[]` for non-inline
 * sources and `canSubmit` gates on `previewPrompts.length > 0`. The fix
 * adds a daemon round-trip (`preview_preset` → `preset_preview`) so the
 * dialog has real prompts to enable the Launch button.
 *
 * This spec validates the daemon side end-to-end: fixture repo with a
 * `folder` preset + 3 `.md` prompts, then drive `preview_preset` through
 * the side-channel WS and assert the response carries those 3 prompts.
 * The client wiring is exercised at typecheck time.
 */
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type {
  DaemonMessage,
  RepoEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

describe("preset launch — folder source", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-preset-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for preset preview e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);

    // .rustling-tulip/presets.json declaring a folder-source preset.
    const presetDir = join(fixtureRepo, ".rustling-tulip");
    await mkdir(presetDir, { recursive: true });
    const preset = [
      {
        id: "folder-preview",
        name: "Folder preview test",
        prompt_sources: [{ kind: "folder", relative_path: "prompts" }],
        prompt_template: "{prompt}",
        context_footer_lines: [],
        variables: [],
        branch_template: "tmp/preview/{stem}",
        session_label_template: "preview {stem}",
        default_use_worktree: true,
        dangerously_skip_permissions: true,
        model: null,
        permission_mode: null,
        tab_grouping: { kind: "none" },
        injector: { startup_delay_ms: 0, pre_input: [], post_input: [] },
        stagger_ms: 0,
      },
    ];
    await writeFile(
      join(presetDir, "presets.json"),
      JSON.stringify(preset, null, 2) + "\n",
    );

    // Three prompt files. The daemon resolves these as `@prompts/<name>.md`
    // sorted by filename.
    const promptsDir = join(fixtureRepo, "prompts");
    await mkdir(promptsDir, { recursive: true });
    await writeFile(join(promptsDir, "alpha.md"), "alpha prompt body\n");
    await writeFile(join(promptsDir, "beta.md"), "beta prompt body\n");
    await writeFile(join(promptsDir, "gamma.md"), "gamma prompt body\n");

    runGit(fixtureRepo, ["add", "."]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
  });

  after(async function () {
    if (ws && registeredRepoId) {
      try {
        ws.send({ type: "remove_repo", repo_id: registeredRepoId });
        await delay(200);
      } catch {
        /* best-effort */
      }
    }
    if (ws) await ws.close();
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
  });

  it("preview_preset returns the 3 folder prompts with @-relative paths", async function () {
    expect(ws, "ws").to.not.equal(null);
    expect(fixtureRepo, "fixtureRepo").to.not.equal(null);
    if (!ws || !fixtureRepo) throw new Error("setup failed");
    const fixturePath = fixtureRepo;

    // Register the fixture repo.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixturePath, name: "rt-e2e-preset-fixture" });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixturePath || r.path === fixturePath.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.not.equal(undefined);
    registeredRepoId = fixture!.id;

    // Send preview_preset for the folder source. The daemon resolves the
    // folder against the preset's source repo path; we pass the absolute
    // folder path that the launch dialog would have built (defaultFolderPath
    // does the same join client-side).
    const folderAbs = join(fixturePath, "prompts");
    const requestId = "test-preview-1";
    const previewPromise = ws.waitFor(isPresetPreviewResponse, { timeoutMs: 5_000 });
    ws.send({
      type: "preview_preset",
      id: requestId,
      target: { kind: "repo", repo_id: registeredRepoId },
      preset_id: "folder-preview",
      source: { kind: "folder", path: folderAbs },
      variable_values: [],
    });
    const response = await previewPromise;

    if (response.type === "preset_preview_error") {
      throw new Error(`preset_preview_error: ${response.error}`);
    }
    expect(response.id).to.equal(requestId);
    expect(response.prompts).to.have.lengthOf(3);
    // Daemon sorts by filename and emits @<rel>/<name>.md. Folder is the
    // absolute fixture path, repo_path is the fixture root, so the
    // relative path is "prompts".
    expect(response.prompts).to.deep.equal([
      "@prompts/alpha.md",
      "@prompts/beta.md",
      "@prompts/gamma.md",
    ]);
  });

  it("preview_preset reports an error for an unknown preset_id", async function () {
    expect(ws, "ws").to.not.equal(null);
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const requestId = "test-preview-bad";
    const previewPromise = ws.waitFor(isPresetPreviewResponse, { timeoutMs: 5_000 });
    ws.send({
      type: "preview_preset",
      id: requestId,
      target: { kind: "repo", repo_id: registeredRepoId },
      preset_id: "does-not-exist",
      source: { kind: "inline", prompts: ["x"] },
      variable_values: [],
    });
    const response = await previewPromise;

    expect(response.type).to.equal("preset_preview_error");
    if (response.type !== "preset_preview_error") return;
    expect(response.id).to.equal(requestId);
    expect(response.error).to.match(/preset not found/i);
  });
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function isPresetPreviewResponse(
  m: DaemonMessage,
): m is
  | { type: "preset_preview"; id: string; prompts: string[] }
  | { type: "preset_preview_error"; id: string; error: string } {
  return m.type === "preset_preview" || m.type === "preset_preview_error";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
