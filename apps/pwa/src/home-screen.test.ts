import { expect, test } from "vitest";

import {
  answer,
  NO_ACTIONS,
  launchLast,
  launchPreset,
  newSession,
  presetKey,
  stop,
  UNSUPPORTED_PRESET_RESEND,
  forgetPresetError,
  type HomeActions,
  type PendingPresetLaunch,
} from "./home-actions";
import { reset, type HomeModel } from "./home-model";
import {
  NO_PRESETS,
  answeredPresetKey,
  foldPresets,
  reconnectPresets,
  togglePresets,
  trackJob,
  type HomePresets,
} from "./home-presets";
import {
  homeScreen,
  homeVisible,
  sameScreen,
  tappedElsewhere,
  updatedSession,
  type HomeState,
} from "./home-screen";
import type {
  DaemonMessage,
  PendingInput,
  PresetEntry,
  PresetLaunchJobSnapshot,
  PresetLaunchJobStatus,
  RepoEntry,
  SessionSnapshot,
  SessionStatus,
  SpawnConfig,
  StatusLabel,
} from "./protocol";

const SINCE = "2026-11-06T12:00:00Z";

function session(id: string, overrides: Partial<SessionSnapshot> = {}): SessionSnapshot {
  return {
    id,
    label: id,
    kind: "single",
    members: [],
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

function waiting(
  id: string,
  prompt: PendingInput = { kind: "permission", tool_name: "Bash", summary: "npm publish" },
): SessionSnapshot {
  return session(id, { status: "awaiting_input", status_since: SINCE, pending_input: prompt });
}

function model(sessions: readonly SessionSnapshot[]): HomeModel {
  return { ...reset(), sessions: [...sessions] };
}

/** A stored launch config for the repo below. */
function spawnConfig(): SpawnConfig {
  return {
    target: {
      kind: "single",
      repo_id: "a",
      branch_name: "main",
      base_branch: null,
      use_worktree: true,
    },
    mode: "interactive",
    agent: null,
  };
}

/** A repo that has launched before. */
const REPO: RepoEntry = {
  id: "a",
  name: "alpha",
  path: "C:\\dev\\alpha",
  default_branch: null,
  last_spawn_config: spawnConfig(),
};

function repoModel(sessions: readonly SessionSnapshot[]): HomeModel {
  const owned = sessions.map((snapshot) => ({
    ...snapshot,
    members:
      snapshot.members.length > 0
        ? snapshot.members
        : [
            {
              repo_id: "a",
              repo_name: "alpha",
              branch: "main",
              worktree_path: "C:\\dev\\alpha",
            },
          ],
  }));
  return { ...model(owned), repos: [REPO] };
}

/** Home's state at rest: ready, paired, shown, on the first card, online. */
function view(overrides: Partial<HomeState> = {}): HomeState {
  return {
    status: "ready",
    paired: true,
    shown: true,
    card: null,
    index: 0,
    offline: false,
    supportsLaunchLast: true,
    supportsNewSession: true,
    newSessionOpen: false,
    ...overrides,
  };
}

function withError(sessionId: string, text: string): HomeActions {
  return { ...NO_ACTIONS, errors: new Map([[sessionId, text]]) };
}

function armed(sessionId: string): HomeActions {
  return { ...NO_ACTIONS, armed: sessionId };
}

/** The first row of the first group, which a model with no repos files under Detached. */
function firstRow(screen: ReturnType<typeof homeScreen>) {
  return screen.groups[0]?.rows[0];
}

test("home_screen_tray_is_hidden_with_no_needs_you", () => {
  expect(
    homeScreen(model([session("s1", { status: "working" })]), NO_ACTIONS, view()).card,
  ).toBeNull();
  expect(homeScreen(model([]), NO_ACTIONS, view()).card).toBeNull();
  for (const quiet of [
    session("parked", { status: "awaiting_input", is_inactive: true }),
    session("gone", { status: "awaiting_input", is_abandoned: true }),
  ]) {
    expect(homeScreen(model([quiet]), NO_ACTIONS, view()).card, quiet.id).toBeNull();
  }
});

test("home_screen_tray_shows_one_card_with_its_label_detail_buttons_and_position", () => {
  const screen = homeScreen(model([waiting("s1")]), NO_ACTIONS, view({ card: "s1" }));

  expect(screen.card).not.toBeNull();
  expect(screen.card?.sessionId).toBe("s1");
  expect(screen.card?.label).toBe("s1");
  expect(screen.card?.detail).toBe("Allow Bash?");
  expect(screen.card?.buttons).toEqual([
    { label: "Allow", hint: null, choice: { kind: "allow" }, enabled: true },
    { label: "Deny", hint: null, choice: { kind: "deny" }, enabled: true },
  ]);
  expect(screen.card?.position).toBe("1 of 1");
  expect(screen.card?.count).toBe(1);
});

test("home_screen_next_and_previous_stop_at_the_ends", () => {
  const two = model([waiting("s1"), waiting("s2", { kind: "other", message: "Ship it?" })]);

  const first = homeScreen(two, NO_ACTIONS, view({ card: "s1" }));
  expect(first.card?.position).toBe("1 of 2");
  expect(first.card?.previous).toBeNull();
  expect(first.card?.next).toBe("s2");

  const second = homeScreen(two, NO_ACTIONS, view({ card: "s2" }));
  expect(second.card?.position).toBe("2 of 2");
  expect(second.card?.previous).toBe("s1");
  expect(second.card?.next).toBeNull();
});

test("home_screen_tracks_the_shown_card_by_id", () => {
  const onB = view({ card: "B", index: 1 });
  const abc = model([waiting("A"), waiting("B"), waiting("C")]);
  expect(homeScreen(abc, NO_ACTIONS, onB).card?.sessionId).toBe("B");

  // A leaves: the shown card keeps its place by id.
  const withoutA = model([waiting("B"), waiting("C")]);
  expect(homeScreen(withoutA, NO_ACTIONS, onB).card?.sessionId).toBe("B");

  // B leaves: the card that took its place, C, shows.
  const withoutB = model([waiting("A"), waiting("C")]);
  expect(homeScreen(withoutB, NO_ACTIONS, onB).card?.sessionId).toBe("C");

  // C leaves while shown: the end of the list falls back to B.
  const onC = view({ card: "C", index: 2 });
  const withoutC = model([waiting("A"), waiting("B")]);
  expect(homeScreen(withoutC, NO_ACTIONS, onC).card?.sessionId).toBe("B");
});

test("home_screen_shows_a_cards_error_text", () => {
  const refused = homeScreen(
    model([waiting("s1")]),
    withError("s1", "the prompt is no longer up"),
    view({ card: "s1" }),
  );
  expect(refused.card?.error).toBe("the prompt is no longer up");

  expect(
    homeScreen(model([waiting("s1")]), NO_ACTIONS, view({ card: "s1" })).card?.error,
  ).toBeNull();
});

test("home_screen_shows_a_rows_error_text", () => {
  const running = model([session("s1", { status: "working" })]);

  expect(firstRow(homeScreen(running, withError("s1", "the session is gone"), view()))?.error).toBe(
    "the session is gone",
  );
  expect(firstRow(homeScreen(running, NO_ACTIONS, view()))?.error).toBeNull();
});

test("home_screen_stop_reads_stop_then_tap_again_to_stop", () => {
  const running = model([session("s1", { status: "working" })]);

  expect(firstRow(homeScreen(running, NO_ACTIONS, view()))?.stop).toBe("Stop");
  expect(firstRow(homeScreen(running, armed("s1"), view()))?.stop).toBe("Tap again to stop");

  const two = model([session("s1", { status: "working" }), session("s2", { status: "working" })]);
  expect(homeScreen(two, armed("s2"), view()).groups[0]?.rows[0]?.stop).toBe("Stop");
});

test("home_screen_an_armed_stop_drops_on_any_other_tap", () => {
  const running = model([session("s1", { status: "working" })]);
  const armedState = armed("s1");
  expect(firstRow(homeScreen(running, armedState, view()))?.stop).toBe("Tap again to stop");

  const afterTap = tappedElsewhere(armedState);
  expect(firstRow(homeScreen(running, afterTap, view()))?.stop).toBe("Stop");
  expect(tappedElsewhere(NO_ACTIONS)).toBe(NO_ACTIONS);
});

test("home_screen_the_second_stop_tap_sends_the_stop", () => {
  const running = model([session("s1", { status: "working" })]);
  const first = stop(NO_ACTIONS, "s1");
  expect(first.message).toBeNull();

  const second = stop(first.state, "s1");
  expect(second.message).toEqual({ type: "stop_session", session_id: "s1", cleanup: [] });
  expect(firstRow(homeScreen(running, second.state, view()))?.stop).toBe("Stop");
});

test("home_screen_an_armed_stop_drops_on_the_sessions_update", () => {
  const armedState = armed("s1");

  expect(updatedSession(armedState, "s1").armed).toBeNull();
  expect(updatedSession(armedState, "s2")).toBe(armedState);

  const running = model([session("s1", { status: "working" })]);
  expect(firstRow(homeScreen(running, updatedSession(armedState, "s1"), view()))?.stop).toBe(
    "Stop",
  );
});

test("home_screen_card_disables_its_answers_while_one_is_pending", () => {
  const asked = model([waiting("s1")]);
  const pressed = answer(NO_ACTIONS, () => "req-1", "s1", SINCE, { kind: "allow" }).state;

  const screen = homeScreen(asked, pressed, view({ card: "s1" }));
  expect(screen.card?.sending).toBe("Sending…");
  for (const choice of screen.card?.buttons ?? []) {
    expect(choice.enabled, choice.label).toBe(false);
  }

  const idle = homeScreen(asked, NO_ACTIONS, view({ card: "s1" }));
  expect(idle.card?.sending).toBeNull();
  expect(idle.card?.buttons.every((choice) => choice.enabled)).toBe(true);
});

test("home_screen_repaints_only_when_a_screen_changed", () => {
  const shown = model([session("s1", { status: "working" })]);
  const first = homeScreen(shown, NO_ACTIONS, view());
  expect(sameScreen(first, homeScreen(shown, NO_ACTIONS, view()))).toBe(true);

  const renamed = model([session("s1", { status: "working", user_label: "Build" })]);
  expect(sameScreen(first, homeScreen(renamed, NO_ACTIONS, view()))).toBe(false);
  expect(sameScreen(first, homeScreen(shown, NO_ACTIONS, view({ offline: true })))).toBe(false);
});

test("home_screen_resume_appears_only_when_offered", () => {
  const stopped = firstRow(
    homeScreen(model([session("s1", { status: "stopped" })]), NO_ACTIONS, view()),
  );
  expect(stopped?.resume).toBe(true);
  expect(stopped?.stop).toBeNull();

  for (const overrides of [
    { status: "error" as const },
    { is_inactive: true },
    { is_abandoned: true },
  ]) {
    const row = firstRow(homeScreen(model([session("s1", overrides)]), NO_ACTIONS, view()));
    expect(row?.resume, JSON.stringify(overrides)).toBe(true);
    expect(row?.stop, JSON.stringify(overrides)).toBeNull();
  }

  const headless = session("s1", { status: "stopped", mode: "headless" });
  expect(firstRow(homeScreen(model([headless]), NO_ACTIONS, view()))?.resume).toBe(false);

  const running = firstRow(
    homeScreen(model([session("s1", { status: "working" })]), NO_ACTIONS, view()),
  );
  expect(running?.resume).toBe(false);
});

test("home_screen_shows_the_status_words", () => {
  const words: Record<SessionStatus, string> = {
    spawning: "Starting",
    idle: "Idle",
    working: "Working",
    awaiting_input: "Needs you",
    stopped: "Stopped",
    error: "Error",
  };

  for (const [status, word] of Object.entries(words)) {
    const row = firstRow(
      homeScreen(
        model([session("s1", { status: status as SessionStatus, user_label: "Build" })]),
        NO_ACTIONS,
        view(),
      ),
    );
    expect(row?.status, status).toBe(word);
    expect(row?.label, status).toBe("Build");
  }
});

test("home_screen_says_no_sessions_when_loaded_and_empty", () => {
  const empty: HomeModel = { ...model([]), loaded: true };
  expect(homeScreen(empty, NO_ACTIONS, view()).empty).toBe("No sessions");

  // Before the first session list: nothing extra.
  expect(homeScreen(model([]), NO_ACTIONS, view()).empty).toBeNull();

  // A loaded list with one session draws its row, not the line.
  const one: HomeModel = { ...model([session("s1")]), loaded: true };
  expect(homeScreen(one, NO_ACTIONS, view()).empty).toBeNull();
});

test("home_screen_shows_the_offline_bar", () => {
  expect(homeScreen(model([]), NO_ACTIONS, view({ offline: true })).bar).toBe("Offline");
  expect(homeScreen(model([]), NO_ACTIONS, view()).bar).toBeNull();
});

test("home_screen_stays_up_while_the_connection_reconnects", () => {
  // Never shown, and not ready and paired: the page's own content stands.
  expect(homeVisible("connecting", false, false)).toBe(false);
  expect(homeVisible("ready", false, false)).toBe(false);
  expect(homeVisible("ready", false, true)).toBe(true);

  // Shown once: reconnecting keeps Home, behind its bar, with every action off.
  const reconnecting = homeScreen(
    model([session("s1", { status: "working" })]),
    NO_ACTIONS,
    view({ status: "backoff" }),
  );
  expect(reconnecting.visible).toBe(true);
  expect(reconnecting.bar).toBe("Reconnecting…");
  expect(reconnecting.actionsEnabled).toBe(false);
  expect(reconnecting.groups[0]?.rows[0]?.enabled).toBe(false);
  expect(reconnecting.groups[0]?.rows[0]?.stop).toBe("Stop");

  // The pairing screen replaces Home.
  expect(homeScreen(model([]), NO_ACTIONS, view({ status: "pairing" })).visible).toBe(false);
  expect(homeScreen(model([]), NO_ACTIONS, view({ status: "notApproved" })).visible).toBe(false);

  // Ready and paired: Home, and no bar.
  expect(homeScreen(model([]), NO_ACTIONS, view()).visible).toBe(true);
  expect(homeScreen(model([]), NO_ACTIONS, view()).bar).toBeNull();
});

/** The status word Home's row shows for `snapshot`. */
function rowWord(snapshot: SessionSnapshot): string | undefined {
  return firstRow(homeScreen(model([snapshot]), NO_ACTIONS, view()))?.status;
}

for (const [label, word] of [
  ["inactive", "Inactive"],
  ["abandoned", "Abandoned"],
  ["orphan", "Orphaned"],
  ["starting", "Starting"],
  ["awaiting_input", "Needs you"],
  ["working", "Working"],
  ["background", "Working in the background"],
  ["idle", "Idle"],
  ["error", "Error"],
  ["stopped", "Stopped"],
] as const) {
  test(`the ${label} label reads ${word}`, () => {
    // The label wins over the status it was computed beside.
    expect(rowWord(session("s1", { status: "idle", status_label: label }))).toBe(word);
  });
}

test("an unknown label falls back to the status word", () => {
  // The wire types leave out `unknown` (the Rust fallback), so these labels come in as raw JSON would.
  const unknown = "unknown" as unknown as StatusLabel;
  expect(rowWord(session("s1", { status: "working", status_label: unknown }))).toBe("Working");
  expect(rowWord(session("s1", { status: "awaiting_input" }))).toBe("Needs you");
  const future = "some_future_label" as unknown as StatusLabel;
  expect(rowWord(session("s1", { status: "stopped", status_label: future }))).toBe("Stopped");
});

test("a spawning status without a label reads Starting", () => {
  expect(rowWord(session("s1", { status: "spawning" }))).toBe("Starting");
});

test("home_screen_shows_the_launch_button_only_on_a_host_that_launches", () => {
  const running = repoModel([session("s1", { status: "working" })]);

  expect(homeScreen(running, NO_ACTIONS, view()).groups[0]?.launch).toEqual({
    target: { kind: "repo", id: "a" },
    label: "Launch last",
    enabled: true,
  });
  expect(
    homeScreen(running, NO_ACTIONS, view({ supportsLaunchLast: false })).groups[0]?.launch,
  ).toBeNull();
});

test("home_screen_shows_a_pending_launch_as_launching", () => {
  const running = repoModel([session("s1", { status: "working" })]);
  const pending = launchLast(NO_ACTIONS, { kind: "repo", id: "a" }, () => "req-1", 0).state;

  const launch = homeScreen(running, pending, view()).groups[0]?.launch;
  expect(launch?.label).toBe("Launching…");
  expect(launch?.enabled).toBe(false);
});

test("home_screen_shows_a_launch_refusal_under_the_header", () => {
  const running = repoModel([session("s1", { status: "working" })]);
  const refused: HomeActions = {
    ...NO_ACTIONS,
    launchErrors: new Map([["repo:a", "alpha has no last launch yet."]]),
  };

  expect(homeScreen(running, refused, view()).groups[0]?.launchError).toBe(
    "alpha has no last launch yet.",
  );
  expect(homeScreen(running, NO_ACTIONS, view()).groups[0]?.launchError).toBeNull();
});

test("home_screen_disables_the_launch_button_while_reconnecting", () => {
  const running = repoModel([session("s1", { status: "working" })]);

  const launch = homeScreen(running, NO_ACTIONS, view({ status: "backoff" })).groups[0]?.launch;
  expect(launch?.label).toBe("Launch last");
  expect(launch?.enabled).toBe(false);
});

test("home_screen_lists_containers_with_a_launch_and_no_sessions", () => {
  const idle: HomeModel = { ...repoModel([]), loaded: true };
  const screen = homeScreen(idle, NO_ACTIONS, view());

  const group = screen.groups[0];
  expect(group?.name).toBe("Launch again");
  expect(group?.rows).toEqual([]);
  expect(group?.launches).toEqual([
    {
      name: "alpha",
      launch: { target: { kind: "repo", id: "a" }, label: "Launch last", enabled: true },
      launchError: null,
    },
  ]);
  expect(screen.empty).toBeNull();
});

test("home_screen_leaves_out_launch_again_on_a_host_that_cannot_launch", () => {
  const idle: HomeModel = { ...repoModel([]), loaded: true };

  const screen = homeScreen(idle, NO_ACTIONS, view({ supportsLaunchLast: false }));
  expect(screen.groups).toEqual([]);
});

/** A preset of repo `a` that launches from the phone: its folder source needs no input. */
const FOLDER_PRESET: PresetEntry = {
  id: "plans",
  name: "Plans",
  source_repo_id: "a",
  prompt_sources: [{ kind: "folder", relative_path: "docs/plans" }],
  prompt_template: "{prompt}",
  branch_template: "plan-{index}",
  tab_grouping: { kind: "none" },
  injector: { steps: [] },
} as unknown as PresetEntry;

/** A preset that needs the desktop: its one source is a file the user picks. */
const FILE_PRESET: PresetEntry = {
  ...FOLDER_PRESET,
  id: "file",
  name: "From a file",
  prompt_sources: [{ kind: "file" }],
};

const ALPHA_REF = { kind: "repo", id: "a" } as const;

/** Repo `a`'s presets list opened and answered with `entries`. */
function listed(entries: readonly PresetEntry[]): HomePresets {
  const opened = togglePresets(NO_PRESETS, ALPHA_REF, () => "list-1").state;
  return foldPresets(opened, {
    type: "presets",
    target: { kind: "repo", repo_id: "a" },
    entries: [...entries],
    request_id: "list-1",
  });
}

/** The folder preset's launch `job-1`, first sent at clock 0. */
const JOB_LAUNCH: PendingPresetLaunch = {
  jobId: "job-1",
  presetId: "plans",
  target: ALPHA_REF,
  source: { kind: "folder", path: "C:\\dev\\alpha\\docs\\plans" },
  sentAt: 0,
  lastSentAt: 0,
};

/** `job-1`'s update carrying `snapshot`. */
function jobUpdate(snapshot: Partial<PresetLaunchJobSnapshot>): DaemonMessage {
  return {
    type: "preset_launch_job_updated",
    job: {
      job_id: "job-1",
      preset_id: "plans",
      target: { kind: "repo", repo_id: "a" },
      ...snapshot,
    },
  };
}

/** `state` after the job `job-1` of the folder preset reports `snapshot`. */
function jobAt(state: HomePresets, snapshot: Partial<PresetLaunchJobSnapshot>): HomePresets {
  return foldPresets(trackJob(state, JOB_LAUNCH), jobUpdate(snapshot));
}

/** `state` after a reconnect at `now`, its open list answered again with the folder preset. */
function reconnected(state: HomePresets, supportsRepeat: boolean, now: number) {
  const ids = ["list-2"];
  const back = reconnectPresets(state, () => ids.shift() ?? "spare", supportsRepeat, now);
  const relisted = foldPresets(back.state, {
    type: "presets",
    target: { kind: "repo", repo_id: "a" },
    entries: [FOLDER_PRESET],
    request_id: "list-2",
  });
  return { messages: back.messages, state: relisted };
}

function presetsOf(presets: HomePresets, actions: HomeActions = NO_ACTIONS) {
  const running = repoModel([session("s1", { status: "working" })]);
  return homeScreen(running, actions, view(), presets).groups[0]?.presets;
}

const LOST = "Lost track of this launch; check the desktop for its sessions.";
const MINUTE = 60 * 1000;

test("home_screen_resends_a_running_job_on_reconnect_and_its_snapshot_ends_the_row", () => {
  const running = jobAt(listed([FOLDER_PRESET]), { status: "resolving", launched: 0, total: 2 });

  const back = reconnected(running, true, 5 * MINUTE);
  expect(back.messages.filter((message) => message.type === "launch_preset")).toEqual([
    {
      type: "launch_preset",
      job_id: "job-1",
      target: { kind: "repo", repo_id: "a" },
      preset_id: "plans",
      source: { kind: "folder", path: "C:\\dev\\alpha\\docs\\plans" },
      variable_values: [],
      use_worktree_override: null,
      max_panes_per_tab_override: null,
    },
  ]);
  // Until the stored snapshot answers, the row keeps its last counts.
  expect(presetsOf(back.state)?.rows[0]?.progress).toBe("Launching 0/2");

  const done = foldPresets(back.state, jobUpdate({ status: "completed", launched: 2, total: 2 }));
  const row = presetsOf(done)?.rows[0];
  expect(row?.progress).toBeNull();
  expect(row?.launch?.enabled).toBe(true);
  expect(row?.note).toBeNull();
});

test("home_screen_ends_a_running_job_locally_on_a_host_that_does_not_repeat", () => {
  const running = jobAt(listed([FOLDER_PRESET]), { status: "resolving", launched: 0, total: 2 });

  const back = reconnected(running, false, 5 * MINUTE);
  expect(back.messages.filter((message) => message.type === "launch_preset")).toEqual([]);
  const row = presetsOf(back.state)?.rows[0];
  expect(row?.progress).toBeNull();
  expect(row?.cancel).toBeNull();
  expect(row?.launch?.enabled).toBe(true);
  expect(row?.note).toBe(LOST);
});

test("home_screen_ends_a_running_job_locally_past_nine_minutes_even_on_a_host_that_repeats", () => {
  const running = jobAt(listed([FOLDER_PRESET]), { status: "spawning", launched: 1, total: 2 });

  const back = reconnected(running, true, 9 * MINUTE);
  expect(back.messages.filter((message) => message.type === "launch_preset")).toEqual([]);
  const row = presetsOf(back.state)?.rows[0];
  expect(row?.progress).toBeNull();
  expect(row?.launch?.enabled).toBe(true);
  expect(row?.note).toBe(LOST);
});

test("home_screen_lets_a_job_s_later_outcome_replace_a_dropped_launch_note", () => {
  const pending = trackJob(listed([FOLDER_PRESET]), JOB_LAUNCH);
  let actions: HomeActions = {
    ...NO_ACTIONS,
    presetErrors: new Map([[presetKey(ALPHA_REF, "plans"), UNSUPPORTED_PRESET_RESEND]]),
  };
  expect(presetsOf(pending, actions)?.rows[0]?.note).toBe(UNSUPPORTED_PRESET_RESEND);

  // Each update of the phone's job is folded as Home folds it: the presets
  // state first, then the note of the preset it answered is cleared.
  const fold = (state: HomePresets, message: DaemonMessage): HomePresets => {
    actions = forgetPresetError(actions, answeredPresetKey(state, message));
    return foldPresets(state, message);
  };
  const failed = fold(pending, jobUpdate({ status: "failed", error: "no prompts to launch" }));
  expect(presetsOf(failed, actions)?.rows[0]?.note).toBe("no prompts to launch");

  const completed = fold(failed, jobUpdate({ status: "completed" }));
  expect(presetsOf(completed, actions)?.rows[0]?.note).toBeNull();
});

test("home_screen_shows_a_closed_presets_toggle_on_a_repo_group", () => {
  expect(presetsOf(NO_PRESETS)).toEqual({
    target: ALPHA_REF,
    label: "Presets",
    open: false,
    enabled: true,
    error: null,
    rows: [],
  });
  // A group that is no repo or workspace offers none.
  expect(homeScreen(model([session("s1")]), NO_ACTIONS, view()).groups[0]?.presets).toBeNull();
});

test("home_screen_offers_launch_for_a_no_input_preset_and_sends_the_rest_to_the_desktop", () => {
  expect(presetsOf(listed([FOLDER_PRESET, FILE_PRESET]))?.rows).toEqual([
    {
      presetId: "plans",
      name: "Plans",
      launch: {
        label: "Launch",
        enabled: true,
        source: { kind: "folder", path: "C:\\dev\\alpha\\docs\\plans" },
      },
      progress: null,
      cancel: null,
      note: null,
    },
    {
      presetId: "file",
      name: "From a file",
      launch: { label: "Needs the desktop", enabled: false, source: null },
      progress: null,
      cancel: null,
      note: null,
    },
  ]);
});

test("home_screen_disables_launch_while_the_preset_launch_is_pending", () => {
  const pending = launchPreset(
    NO_ACTIONS,
    { target: ALPHA_REF, presetId: "plans", source: { kind: "inline", prompts: ["Plans"] } },
    () => "req-1",
    0,
  ).state;

  expect(presetsOf(listed([FOLDER_PRESET]), pending)?.rows[0]?.launch?.enabled).toBe(false);
});

test("home_screen_shows_a_running_job_as_progress_with_cancel", () => {
  const row = presetsOf(
    jobAt(listed([FOLDER_PRESET]), { status: "spawning", launched: 1, total: 3 }),
  )?.rows[0];

  expect(row?.progress).toBe("Launching 1/3");
  expect(row?.cancel).toBe("job-1");
  expect(row?.launch).toBeNull();
});

test("home_screen_shows_the_last_count_for_a_job_status_it_does_not_know", () => {
  const later = jobAt(listed([FOLDER_PRESET]), { status: "spawning", launched: 2, total: 4 });
  const unknown = foldPresets(later, {
    type: "preset_launch_job_updated",
    job: {
      job_id: "job-1",
      preset_id: "plans",
      target: { kind: "repo", repo_id: "a" },
      status: "paused_for_later" as unknown as PresetLaunchJobStatus,
    },
  });

  expect(presetsOf(unknown)?.rows[0]?.progress).toBe("Launching 2/4");
});

test("home_screen_shows_a_failed_job_s_error_verbatim", () => {
  const failed = jobAt(listed([FOLDER_PRESET]), {
    status: "failed",
    error: "no prompts to launch",
  });
  const row = presetsOf(failed)?.rows[0];

  expect(row?.note).toBe("no prompts to launch");
  expect(row?.progress).toBeNull();
  expect(row?.launch?.label).toBe("Launch");
});

test("home_screen_shows_a_cancelled_job_as_cancelled", () => {
  expect(presetsOf(jobAt(listed([FOLDER_PRESET]), { status: "cancelled" }))?.rows[0]?.note).toBe(
    "Cancelled",
  );
});

test("home_screen_returns_a_completed_job_to_its_launch_button", () => {
  const row = presetsOf(
    jobAt(listed([FOLDER_PRESET]), { status: "completed", launched: 3, total: 3 }),
  )?.rows[0];

  expect(row?.launch).toEqual({
    label: "Launch",
    enabled: true,
    source: { kind: "folder", path: "C:\\dev\\alpha\\docs\\plans" },
  });
  expect(row?.progress).toBeNull();
  expect(row?.note).toBeNull();
});

test("home_screen_shows_a_dropped_preset_launch_s_text_on_its_row", () => {
  const dropped: HomeActions = {
    ...NO_ACTIONS,
    presetErrors: new Map([[presetKey(ALPHA_REF, "plans"), UNSUPPORTED_PRESET_RESEND]]),
  };

  expect(presetsOf(listed([FOLDER_PRESET]), dropped)?.rows[0]?.note).toBe(
    "This host can't resend a preset launch; check the desktop.",
  );
});

test("home_screen_shows_a_refused_presets_list_s_message", () => {
  const opened = togglePresets(NO_PRESETS, ALPHA_REF, () => "list-1").state;
  const refused = foldPresets(opened, {
    type: "error",
    request_id: "list-1",
    code: "not_found",
    message: "No such repo or workspace.",
  });

  expect(presetsOf(refused)?.error).toBe("No such repo or workspace.");
  expect(presetsOf(refused)?.rows).toEqual([]);
});

test("home_screen_says_no_sessions_when_only_hidden_launches_would_show", () => {
  const idle: HomeModel = { ...repoModel([]), loaded: true };

  expect(homeScreen(idle, NO_ACTIONS, view({ supportsLaunchLast: false })).empty).toBe(
    "No sessions",
  );
  // With the feature the group is content, so the line stays away.
  expect(homeScreen(idle, NO_ACTIONS, view()).empty).toBeNull();
});

/** A repo that has never launched: it makes no group, so a loaded model of it alone is empty. */
const BETA: RepoEntry = { id: "b", name: "beta", path: "C:\\dev\\beta", default_branch: null };

test("home_screen_shows_new_session_with_no_sessions_and_with_groups", () => {
  const empty: HomeModel = { ...model([]), repos: [BETA], loaded: true };
  const screen = homeScreen(empty, NO_ACTIONS, view());
  expect(screen.empty).toBe("No sessions");
  expect(screen.newSession).toEqual({ label: "New session", open: false, enabled: true, rows: [] });

  expect(homeScreen(empty, NO_ACTIONS, view({ newSessionOpen: true })).newSession?.rows).toEqual([
    { target: { kind: "repo", id: "b" }, name: "beta", label: "Start", enabled: true, error: null },
  ]);

  const running: HomeModel = { ...repoModel([session("s1", { status: "working" })]), loaded: true };
  const withGroups = homeScreen(running, NO_ACTIONS, view());
  expect(withGroups.groups).toHaveLength(1);
  expect(withGroups.newSession?.label).toBe("New session");
});

test("home_screen_hides_new_session_on_a_host_without_it_and_before_the_model_loads", () => {
  const empty: HomeModel = { ...model([]), repos: [BETA], loaded: true };
  expect(homeScreen(empty, NO_ACTIONS, view({ supportsNewSession: false })).newSession).toBeNull();

  const unloaded: HomeModel = { ...model([]), repos: [BETA] };
  expect(homeScreen(unloaded, NO_ACTIONS, view()).newSession).toBeNull();
});

test("home_screen_shows_a_dropped_new_session_error_on_a_host_without_it", () => {
  const running: HomeModel = {
    ...repoModel([session("s1", { status: "working" })]),
    repos: [REPO, BETA],
    loaded: true,
  };
  const dropped: HomeActions = {
    ...NO_ACTIONS,
    newSessionErrors: new Map([["repo:a", "This host can't launch from the phone."]]),
  };

  const screen = homeScreen(running, dropped, view({ supportsNewSession: false }));
  expect(screen.newSession).toEqual({
    label: null,
    open: true,
    enabled: false,
    rows: [
      {
        target: { kind: "repo", id: "a" },
        name: "alpha",
        label: null,
        enabled: false,
        error: "This host can't launch from the phone.",
      },
    ],
  });
  // Never under the group header.
  expect(screen.groups[0]?.launchError).toBeNull();
});

test("home_screen_disables_new_session_while_reconnecting", () => {
  const empty: HomeModel = { ...model([]), repos: [BETA], loaded: true };

  const picker = homeScreen(
    empty,
    NO_ACTIONS,
    view({ status: "backoff", newSessionOpen: true }),
  ).newSession;
  expect(picker?.enabled).toBe(false);
  expect(picker?.rows.map((row) => row.enabled)).toEqual([false]);
});

test("home_screen_shows_one_pending_launch_per_container_by_its_own_kind", () => {
  const running: HomeModel = {
    ...repoModel([session("s1", { status: "working" })]),
    repos: [REPO, BETA],
    loaded: true,
  };
  const alpha = { kind: "repo" as const, id: "a" };

  // A New session waits: its row says so, and Launch last is off but keeps its own label.
  const starting = newSession(NO_ACTIONS, alpha, () => "req-1", 0).state;
  const one = homeScreen(running, starting, view({ newSessionOpen: true }));
  expect(one.newSession?.rows.map((row) => [row.name, row.label, row.enabled])).toEqual([
    ["alpha", "Starting…", false],
    ["beta", "Start", true],
  ]);
  expect(one.groups[0]?.launch).toEqual({ target: alpha, label: "Launch last", enabled: false });

  // A Launch last waits: its button says so, and the row's Start is off.
  const launching = launchLast(NO_ACTIONS, alpha, () => "req-1", 0).state;
  const two = homeScreen(running, launching, view({ newSessionOpen: true }));
  expect(two.newSession?.rows[0]).toMatchObject({ label: "Start", enabled: false });
  expect(two.groups[0]?.launch).toEqual({ target: alpha, label: "Launching…", enabled: false });
});

test("home_screen_shows_a_new_session_refusal_under_its_picker_row_not_the_header", () => {
  const running: HomeModel = { ...repoModel([session("s1", { status: "working" })]), loaded: true };
  const refused: HomeActions = {
    ...NO_ACTIONS,
    newSessionErrors: new Map([["repo:a", "No such repo or workspace."]]),
  };

  const screen = homeScreen(running, refused, view({ newSessionOpen: true }));
  expect(screen.newSession?.rows[0]?.error).toBe("No such repo or workspace.");
  expect(screen.groups[0]?.launchError).toBeNull();
});
