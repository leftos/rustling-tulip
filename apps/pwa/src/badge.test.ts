import { afterEach, expect, test, vi } from "vitest";

import { createBadge, type BadgeEnv } from "./badge";
import { homeStep, reset, type HomeModel } from "./home-model";
import type { SessionSnapshot } from "./protocol";

const SINCE = "2026-11-06T12:00:00Z";

function session(id: string, overrides: Partial<SessionSnapshot> = {}): SessionSnapshot {
  return {
    id,
    label: id,
    kind: "single",
    members: [
      { repo_id: "a", repo_name: "alpha", branch: "main", worktree_path: "C:\\dev\\alpha" },
    ],
    status: "idle",
    mode: "interactive",
    started_at: "2026-11-06T10:00:00Z",
    exit_code: null,
    metrics: { input_tokens: 0, output_tokens: 0, cost_usd: 0, last_activity_at: null },
    recent_actions: [],
    agent: null,
    ...overrides,
  };
}

/** A session the Needs you tray lists. */
function waiting(id: string): SessionSnapshot {
  return session(id, { status: "awaiting_input", status_since: SINCE });
}

/** The model after the daemon's first `sessions` message. */
function loaded(...sessions: SessionSnapshot[]): HomeModel {
  return homeStep(reset(), { type: "sessions", sessions });
}

function spyEnv() {
  return {
    set: vi.fn(async (_count: number) => undefined),
    clear: vi.fn(async () => undefined),
  } satisfies BadgeEnv;
}

/** Lets the callbacks of the promise the badge queued run. */
async function settle(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

afterEach(() => {
  vi.restoreAllMocks();
});

test("badge_does_nothing_before_sessions_load", async () => {
  const env = spyEnv();
  const badge = createBadge(env);

  badge.update(reset());
  await settle();

  expect(env.set).not.toHaveBeenCalled();
  expect(env.clear).not.toHaveBeenCalled();
});

test("badge_sets_the_count_from_the_first_loaded_model", async () => {
  const env = spyEnv();
  const badge = createBadge(env);

  badge.update(loaded(waiting("s1"), waiting("s2")));
  await settle();

  expect(env.set).toHaveBeenCalledTimes(1);
  expect(env.set).toHaveBeenCalledWith(2);
  expect(env.clear).not.toHaveBeenCalled();
});

test("badge_clears_the_badge_when_the_first_loaded_model_has_nobody_waiting", async () => {
  const env = spyEnv();
  const badge = createBadge(env);

  badge.update(loaded(session("s1")));
  await settle();

  expect(env.clear).toHaveBeenCalledTimes(1);
  expect(env.set).not.toHaveBeenCalled();
});

test("badge_does_not_repeat_an_unchanged_count", async () => {
  const env = spyEnv();
  const badge = createBadge(env);

  badge.update(loaded(waiting("s1")));
  await settle();
  badge.update(loaded(waiting("s1")));
  await settle();

  expect(env.set).toHaveBeenCalledTimes(1);
  expect(env.set).toHaveBeenCalledWith(1);
});

test("badge_clears_when_the_count_drops_to_zero", async () => {
  const env = spyEnv();
  const badge = createBadge(env);

  badge.update(loaded(waiting("s1")));
  await settle();
  badge.update(loaded(session("s1")));
  await settle();

  expect(env.set).toHaveBeenCalledWith(1);
  expect(env.clear).toHaveBeenCalledTimes(1);
});

test("badge_does_nothing_without_the_badging_api", async () => {
  const badge = createBadge(null);

  expect(() => {
    badge.update(loaded(waiting("s1")));
  }).not.toThrow();
  await settle();
});

test("badge_a_refused_call_is_logged_and_the_next_change_still_applies", async () => {
  const env = spyEnv();
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  env.set.mockRejectedValueOnce(new Error("denied"));
  const badge = createBadge(env);

  badge.update(loaded(waiting("s1")));
  await settle();

  expect(warn).toHaveBeenCalledWith("setAppBadge failed", expect.anything());

  badge.update(loaded(waiting("s1")));
  await settle();

  expect(env.set).toHaveBeenCalledTimes(2);
  expect(env.set).toHaveBeenLastCalledWith(1);
});
