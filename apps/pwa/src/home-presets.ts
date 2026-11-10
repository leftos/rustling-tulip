import {
  LAUNCH_RESEND_LIMIT_MS,
  containerKey,
  presetKey,
  presetLaunchMessage,
  presetTarget,
  type NewRequestId,
  type PendingPresetLaunch,
} from "./home-actions";
import type {
  AgentDescriptor,
  AgentSelection,
  ClientMessage,
  ContainerRef,
  DaemonMessage,
  LaunchPresetSource,
  PresetEntry,
  PresetLaunchJobSnapshot,
  PresetPromptSource,
  PresetVariable,
  RepoEntry,
} from "./protocol";

/**
 * The phone's presets: which presets a group lists, which of them launch
 * without asking anything, and the jobs the phone started. Pure; Home's view
 * only folds messages through it and renders what `home-screen.ts` makes of it.
 */

/** What a preset the phone cannot launch says on its disabled button. */
export const NEEDS_DESKTOP = "Needs the desktop";

/** The variable kinds that resolve from their defaults on the PC without asking. */
const QUIET_KINDS: ReadonlySet<string> = new Set([
  "text",
  "file_path",
  "folder_path",
  "env_var",
  "literal_path",
  "toggle",
]);

/**
 * The source the phone launches `preset` with, or null when it needs the
 * desktop: a variable asked at launch, a script or unknown variable, the
 * agent's bypass choice, or no source the phone can fill without asking.
 * `agents` is null until the host's `agents` message has arrived.
 */
export function phoneLaunch(
  preset: PresetEntry,
  repos: readonly RepoEntry[],
  agents: readonly AgentDescriptor[] | null,
): LaunchPresetSource | null {
  if ((preset.variables ?? []).some(asksOrRuns) || mayBypass(preset.agent ?? null, agents)) {
    return null;
  }
  const repo = repos.find((known) => known.id === preset.source_repo_id);
  for (const source of preset.prompt_sources) {
    const filled = fillSource(preset, source, repo);
    if (filled !== null) {
      return filled;
    }
  }
  return null;
}

/** A source the phone fills without asking; null for one that needs typing or picking. */
function fillSource(
  preset: PresetEntry,
  source: PresetPromptSource,
  repo: RepoEntry | undefined,
): LaunchPresetSource | null {
  if (source.kind === "folder") {
    return repo === undefined
      ? null
      : { kind: "folder", path: joinPath(repo.path, source.relative_path) };
  }
  if (source.kind === "inline" && !preset.prompt_template.includes("{prompt}")) {
    return { kind: "inline", prompts: [preset.name] };
  }
  return null;
}

/** A variable the launch would ask for, or one that runs a program on the PC unreviewed. */
function asksOrRuns(variable: PresetVariable): boolean {
  return variable.prompt_at_launch === true || !QUIET_KINDS.has(variable.kind.kind);
}

/**
 * Whether the preset's agent options hold that agent's bypass choice; options
 * the phone cannot check yet, before the descriptors arrive, count as bypass.
 */
function mayBypass(
  agent: AgentSelection | null,
  agents: readonly AgentDescriptor[] | null,
): boolean {
  if (agent === null) {
    return false;
  }
  if (agents === null) {
    return Object.keys(agent.options).length > 0;
  }
  const bypass = agents.find((descriptor) => descriptor.id === agent.agent)?.bypass ?? null;
  return bypass !== null && agent.options[bypass.field] === bypass.value;
}

/**
 * `rel` under `base`, as the desktop's `join_path` joins a folder source: a
 * base starting with a drive letter or `\\` is Windows and joins with `\`,
 * any other with `/`, and stray separators at the seam are dropped.
 */
export function joinPath(base: string, rel: string): string {
  const windows = /^[A-Za-z]:/.test(base) || base.startsWith("\\\\");
  const trimmedBase = base.replace(/[/\\]+$/, "");
  const trimmedRel = rel.replace(/^[/\\]+/, "");
  if (trimmedRel === "") {
    return trimmedBase;
  }
  return windows
    ? `${trimmedBase}\\${trimmedRel.replaceAll("/", "\\")}`
    : `${trimmedBase}/${trimmedRel.replaceAll("\\", "/")}`;
}

/** One container's presets list: on its way, listed, or refused with the daemon's text. */
export type PresetList =
  | { readonly kind: "loading" }
  | { readonly kind: "listed"; readonly entries: readonly PresetEntry[] }
  | { readonly kind: "refused"; readonly error: string };

/** A preset job the phone started, as its latest message left it. */
export interface PresetJob {
  readonly jobId: string;
  readonly presetId: string;
  readonly target: ContainerRef;
  /** The container's `containerKey`. */
  readonly key: string;
  /** The source the launch sent, which a resend sends again. */
  readonly source: LaunchPresetSource;
  /** When the launch's first send went out, ms since the epoch: the journal clock. */
  readonly sentAt: number;
  /**
   * The job's status word; null before its first update, and `lost` once a
   * reconnect could not ask the host about it again.
   */
  readonly status: string | null;
  readonly launched: number;
  readonly total: number;
  readonly error: string | null;
}

/** What Home keeps about presets between messages. */
export interface HomePresets {
  /** Each container's list, by `containerKey`, fetched on this connection. */
  readonly lists: ReadonlyMap<string, PresetList>;
  /** The container each `list_presets` on its way asked for, by its `request_id`. */
  readonly requests: ReadonlyMap<string, string>;
  /** The containers whose presets list is open, by `containerKey`. */
  readonly open: ReadonlyMap<string, ContainerRef>;
  /** The host's agent descriptors; null until its `agents` message arrives. */
  readonly agents: readonly AgentDescriptor[] | null;
  /** The jobs the phone started, oldest first. */
  readonly jobs: readonly PresetJob[];
}

/** The presets state before any message. */
export const NO_PRESETS: HomePresets = {
  lists: new Map(),
  requests: new Map(),
  open: new Map(),
  agents: null,
  jobs: [],
};

/** What a reconnect sends for presets: a fresh list for each open container. */
export interface PresetsReconnect {
  readonly messages: readonly ClientMessage[];
  readonly state: HomePresets;
}

/**
 * The state a fresh connection starts from: every list and the descriptors
 * come again, so each open container is listed afresh; the open lists and
 * the phone's jobs stay.
 */
export function reconnectPresets(
  state: HomePresets,
  newRequestId: NewRequestId,
  supportsRepeat: boolean,
  now: number,
): PresetsReconnect {
  const resumed = resumeJobs(state.jobs, supportsRepeat, now);
  let next: HomePresets = { ...NO_PRESETS, open: state.open, jobs: resumed.jobs };
  const messages: ClientMessage[] = [...resumed.messages];
  for (const target of state.open.values()) {
    const fetched = fetchList(next, target, newRequestId);
    next = fetched.state;
    messages.push(fetched.message);
  }
  return { messages, state: next };
}

/**
 * The phone's running jobs after a reconnect: the host does not push their
 * snapshots to a new connection, so each is asked about again with its
 * launch under the same job id, which a host listing `preset_job_repeat`
 * answers with the stored snapshot and starts nothing. A job older than the
 * journal, or on a host that cannot repeat, ends here as lost. A job not
 * yet answered is its pending launch's to resend; an ended one stays.
 */
function resumeJobs(
  jobs: readonly PresetJob[],
  supportsRepeat: boolean,
  now: number,
): { readonly jobs: readonly PresetJob[]; readonly messages: readonly ClientMessage[] } {
  const messages: ClientMessage[] = [];
  const next = jobs.map((job) => {
    if (job.status === null || jobEnded(job)) {
      return job;
    }
    if (supportsRepeat && now - job.sentAt < LAUNCH_RESEND_LIMIT_MS) {
      messages.push(presetLaunchMessage(job));
      return job;
    }
    return { ...job, status: LOST_STATUS };
  });
  return { jobs: next, messages };
}

/** What a preset row says once a reconnect lost track of its running job. */
export const LOST_JOB = "Lost track of this launch; check the desktop for its sessions.";

/** The status a job the phone lost track of ends in; no daemon sends it. */
export const LOST_STATUS = "lost";

/** The job statuses after which a preset row shows its button again. */
const JOB_ENDED: ReadonlySet<string> = new Set(["completed", "failed", "cancelled", LOST_STATUS]);

/** What a tap on a group's Presets toggle produces: the fetch to send, if any, and the state. */
export interface PresetsToggle {
  readonly message: ClientMessage | null;
  readonly state: HomePresets;
}

/**
 * Opens or closes a container's presets list. The first open on a connection
 * also asks the host for the list; later opens show what it answered.
 */
export function togglePresets(
  state: HomePresets,
  target: ContainerRef,
  newRequestId: NewRequestId,
): PresetsToggle {
  const key = containerKey(target);
  const open = new Map(state.open);
  if (open.delete(key)) {
    return { message: null, state: { ...state, open } };
  }
  open.set(key, target);
  if (state.lists.has(key)) {
    return { message: null, state: { ...state, open } };
  }
  return fetchList({ ...state, open }, target, newRequestId);
}

/** The `list_presets` for one container, its list marked as on its way. */
function fetchList(
  state: HomePresets,
  target: ContainerRef,
  newRequestId: NewRequestId,
): { readonly message: ClientMessage; readonly state: HomePresets } {
  const key = containerKey(target);
  const requestId = newRequestId();
  const lists = new Map(state.lists);
  lists.set(key, { kind: "loading" });
  const requests = new Map(state.requests);
  requests.set(requestId, key);
  return {
    message: { type: "list_presets", target: presetTarget(target), request_id: requestId },
    state: { ...state, lists, requests },
  };
}

/** Starts following a job the phone just minted, so its updates are read. */
export function trackJob(state: HomePresets, launch: PendingPresetLaunch): HomePresets {
  const job: PresetJob = {
    jobId: launch.jobId,
    presetId: launch.presetId,
    target: launch.target,
    key: containerKey(launch.target),
    source: launch.source,
    sentAt: launch.sentAt,
    status: null,
    launched: 0,
    total: 0,
    error: null,
  };
  return { ...state, jobs: [...state.jobs.filter((known) => known.jobId !== launch.jobId), job] };
}

/** Whether a job has stopped: completed, failed, cancelled, or lost track of. */
export function jobEnded(job: PresetJob): boolean {
  return JOB_ENDED.has(job.status ?? "");
}

/**
 * The `presetKey` of the preset whose phone job `message` updates, so its
 * dropped-launch note gives way to the job's outcome; null for any other message.
 */
export function answeredPresetKey(state: HomePresets, message: DaemonMessage): string | null {
  const jobId =
    message.type === "preset_launch_job_updated"
      ? message.job.job_id
      : message.type === "preset_launch_failed"
        ? message.job_id
        : undefined;
  const job = state.jobs.find((known) => known.jobId === jobId);
  return job === undefined ? null : presetKey(job.target, job.presetId);
}

/** The latest job the phone started for one preset of one container; undefined when none. */
export function jobFor(state: HomePresets, key: string, presetId: string): PresetJob | undefined {
  return state.jobs.findLast((job) => job.key === key && job.presetId === presetId);
}

/**
 * One daemon message folded into the presets state. Pure; a message it does
 * not read, or one about a list or a job the phone did not ask for, returns
 * the state itself.
 */
export function foldPresets(state: HomePresets, message: DaemonMessage): HomePresets {
  switch (message.type) {
    case "agents":
      return { ...state, agents: message.agents };
    case "presets":
      return answerList(state, message.request_id ?? null, {
        kind: "listed",
        entries: message.entries,
      });
    case "error":
      return answerList(state, message.request_id ?? null, {
        kind: "refused",
        error: message.message,
      });
    case "preset_launch_job_updated":
      return withJobUpdate(state, message.job);
    case "preset_launch_failed":
      return withJob(state, message.job_id ?? "", (job) => ({
        ...job,
        status: "failed",
        error: message.error,
      }));
    default:
      return state;
  }
}

/** A list's answer, filed under the container its request asked for. */
function answerList(state: HomePresets, requestId: string | null, list: PresetList): HomePresets {
  const key = requestId === null ? undefined : state.requests.get(requestId);
  if (requestId === null || key === undefined) {
    return state;
  }
  const lists = new Map(state.lists);
  lists.set(key, list);
  const requests = new Map(state.requests);
  requests.delete(requestId);
  return { ...state, lists, requests };
}

function withJobUpdate(state: HomePresets, snapshot: PresetLaunchJobSnapshot): HomePresets {
  return withJob(state, snapshot.job_id, (job) => ({
    ...job,
    status: snapshot.status ?? job.status,
    launched: snapshot.launched ?? job.launched,
    total: snapshot.total ?? job.total,
    error: snapshot.error ?? null,
  }));
}

/** The state with one of the phone's jobs changed; the state itself for any other job id. */
function withJob(
  state: HomePresets,
  jobId: string,
  change: (job: PresetJob) => PresetJob,
): HomePresets {
  const at = state.jobs.findIndex((job) => job.jobId === jobId);
  if (at === -1) {
    return state;
  }
  const jobs = state.jobs.slice();
  const job = jobs[at];
  if (job !== undefined) {
    jobs[at] = change(job);
  }
  return { ...state, jobs };
}
