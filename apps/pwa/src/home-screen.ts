import { RECONNECTING_BAR, connectionBar } from "./connection-bar";
import type { ConnectionStateName } from "./connection-machine";
import {
  containerKey,
  disarm,
  errorFor,
  pendingLaunchKind,
  presetKey,
  type HomeActions,
} from "./home-actions";
import {
  homeGroups,
  needsYou,
  newSessionChoices,
  type HomeGroup,
  type HomeModel,
  type NewSessionChoice,
  type TrayCard,
} from "./home-model";
import {
  LOST_JOB,
  LOST_STATUS,
  NEEDS_DESKTOP,
  NO_PRESETS,
  jobEnded,
  jobFor,
  phoneLaunch,
  type HomePresets,
  type PresetJob,
} from "./home-presets";
import type {
  ContainerRef,
  LaunchPresetSource,
  PresetEntry,
  PromptChoice,
  SessionSnapshot,
  SessionStatus,
} from "./protocol";

/**
 * What Home draws: a pure view model over the Home model, the actions state
 * and the connection's state, so the texts and the visible parts are
 * testable without a DOM. `home-view.ts` only renders it.
 */

/** The statuses a session shows Stop in: it is up, or being started. */
const LIVE: ReadonlySet<SessionStatus> = new Set(["spawning", "working", "awaiting_input", "idle"]);

/** The statuses where the pairing screen owns the page; Home never stands there. */
const PAIRING_SCREEN: ReadonlySet<ConnectionStateName> = new Set(["pairing", "notApproved"]);

/** The Stop button's label, before and while it waits for its second tap. */
export const STOP = "Stop";
export const STOP_ARMED = "Tap again to stop";

/** What Home says once the host's session list has loaded with nothing in it. */
export const NO_SESSIONS = "No sessions";

/** What a card says while its answer is on its way to the daemon. */
export const SENDING = "Sending…";

/** What a group's Launch last button says, before and while its launch waits. */
export const LAUNCH_LAST = "Launch last";
export const LAUNCHING = "Launching…";

/** A group's presets toggle, a preset's buttons, and what an ended job leaves on its row. */
export const PRESETS = "Presets";
export const LAUNCH_PRESET = "Launch";
export const CANCEL = "Cancel";
export const CANCELLED = "Cancelled";

/** Home's New session button, and a picker row's button before and while its session starts. */
export const NEW_SESSION = "New session";
export const START = "Start";
export const STARTING = "Starting…";

/** One container of the New session picker. */
export interface NewSessionRowView {
  readonly target: ContainerRef;
  readonly name: string;
  /** The row's button; null on a host without `new_session`, where the row only says why. */
  readonly label: "Start" | "Starting…" | null;
  readonly enabled: boolean;
  /** The daemon's refusal of this container's New session, verbatim; null when none. */
  readonly error: string | null;
}

/** Home's New session button and, while it is open, the containers it offers. */
export interface NewSessionView {
  /**
   * The button; null on a host without `new_session`, where the view only
   * lists the containers whose New session it dropped, each with its error.
   */
  readonly label: "New session" | null;
  readonly open: boolean;
  readonly enabled: boolean;
  /** The containers, while the picker is open; empty otherwise. */
  readonly rows: readonly NewSessionRowView[];
}

/** One Launch last button, as a group header or a Launch again row shows it. */
export interface LaunchView {
  readonly target: ContainerRef;
  readonly label: "Launch last" | "Launching…";
  readonly enabled: boolean;
}

/** One row of the Launch again group: a container with nothing running. */
export interface LaunchRowView {
  readonly name: string;
  readonly launch: LaunchView | null;
  readonly launchError: string | null;
}

/** One answer button of a Needs you card. */
export interface CardButton {
  readonly label: string;
  /** The key hint, `1` to `n`; null when the choice shows none. */
  readonly hint: string | null;
  readonly choice: PromptChoice;
  readonly enabled: boolean;
}

/** The one Needs you card the tray shows. */
export interface CardView {
  readonly sessionId: string;
  readonly label: string;
  readonly detail: string;
  /** The `status_since` an answer is sent against; null without one. */
  readonly since: string | null;
  /** Where the card sits among the cards, as `1 of 3`. */
  readonly position: string;
  /** How many cards there are. */
  readonly count: number;
  readonly buttons: readonly CardButton[];
  /** The button that opens the session, or null when the card answers. */
  readonly open: string | null;
  readonly openEnabled: boolean;
  /** The daemon's refusal text for the session, when it refused one. */
  readonly error: string | null;
  /** The line saying the answer is on its way; null when none is. */
  readonly sending: string | null;
  /** The card Previous moves to; null when there is none, or actions are off. */
  readonly previous: string | null;
  /** The card Next moves to; null when there is none, or actions are off. */
  readonly next: string | null;
}

/** One row of a group. */
export interface RowView {
  readonly sessionId: string;
  readonly label: string;
  /** The row's status word. */
  readonly status: string;
  /** The Stop button's label while it is offered; null when the row offers none. */
  readonly stop: string | null;
  /** Whether the row offers Resume, its one other action. */
  readonly resume: boolean;
  /** False while the connection is coming back: the row's tap and buttons do nothing. */
  readonly enabled: boolean;
  /** The daemon's refusal text for this row, when it refused one. */
  readonly error: string | null;
}

/** A preset row's Launch button, or its disabled `Needs the desktop` one. */
export interface PresetLaunchView {
  readonly label: "Launch" | "Needs the desktop";
  readonly enabled: boolean;
  /** What the launch sends as its source; null when the preset needs the desktop. */
  readonly source: LaunchPresetSource | null;
}

/** One preset of a group's presets list. */
export interface PresetRowView {
  readonly presetId: string;
  readonly name: string;
  /** The row's button; null while the phone's job for it runs. */
  readonly launch: PresetLaunchView | null;
  /** `Launching 1/3` while the job runs; null otherwise. */
  readonly progress: string | null;
  /** The running job the row's Cancel stops; null when it offers none. */
  readonly cancel: string | null;
  /** The job's error, `Cancelled`, or why its launch was dropped; null when there is none. */
  readonly note: string | null;
}

/** A repo's or workspace's presets: the toggle that opens the list, and the list. */
export interface PresetsView {
  readonly target: ContainerRef;
  readonly label: "Presets";
  readonly open: boolean;
  readonly enabled: boolean;
  /** The daemon's refusal of the list, verbatim; null when it listed or has not answered. */
  readonly error: string | null;
  /** The presets, while the list is open and listed; empty otherwise. */
  readonly rows: readonly PresetRowView[];
}

/** A group of rows: a workspace, a repo, or one of the single buckets. */
export interface GroupView {
  readonly name: string;
  readonly rows: readonly RowView[];
  /** The group's presets; null for a group that is no repo or workspace. */
  readonly presets: PresetsView | null;
  /** The header's Launch last button; null when the group offers none. */
  readonly launch: LaunchView | null;
  /** The daemon's refusal text for this group's launch, when it refused one. */
  readonly launchError: string | null;
  /** The containers a Launch again group lists; empty for every other group. */
  readonly launches: readonly LaunchRowView[];
}

/** What Home knows about the connection and where the tray stands. */
export interface HomeState {
  readonly status: ConnectionStateName;
  /** The ready connection presented a device token. */
  readonly paired: boolean;
  /** Home has shown at some point in this page visit. */
  readonly shown: boolean;
  /** The tray's shown card, by session id; null before one is chosen. */
  readonly card: string | null;
  /** The place the shown card had last, which its successor inherits when it leaves. */
  readonly index: number;
  /** A send has been refused since the last `ready`. */
  readonly offline: boolean;
  /** The host's `Welcome.features` lists `launch_last`. */
  readonly supportsLaunchLast: boolean;
  /** The host's `Welcome.features` lists `new_session`. */
  readonly supportsNewSession: boolean;
  /** The New session picker is open. */
  readonly newSessionOpen: boolean;
}

/** Everything Home draws. */
export interface HomeScreen {
  readonly visible: boolean;
  /** `Reconnecting…` or `Offline`; null when the connection is fine. */
  readonly bar: string | null;
  /** False while the connection is coming back: every action button is disabled. */
  readonly actionsEnabled: boolean;
  /** The shown card's place among the cards; 0 when there are none. */
  readonly index: number;
  /** The tray's card, or null when nothing needs the user. */
  readonly card: CardView | null;
  readonly groups: readonly GroupView[];
  /** `No sessions` once the list has loaded empty; null before it loads, and with any session. */
  readonly empty: string | null;
  /** The New session button, above the groups; null before the list loads or on a host without it. */
  readonly newSession: NewSessionView | null;
}

/**
 * Whether Home stands: once it has shown in this page visit it survives every
 * status but the pairing screen's own; before that it needs a ready, paired
 * connection.
 */
export function homeVisible(status: ConnectionStateName, shown: boolean, paired: boolean): boolean {
  if (PAIRING_SCREEN.has(status)) {
    return false;
  }
  return shown || (status === "ready" && paired);
}

/** The bar Home shows at `state`: the reconnecting one wins over a refused send. */
export function homeBar(state: HomeState): string | null {
  return connectionBar(state.status, state.shown, state.offline);
}

/** The card index to show for `index` among `count` cards; 0 when there are none. */
export function clampCard(index: number, count: number): number {
  if (count <= 0 || index < 0) {
    return 0;
  }
  return Math.min(index, count - 1);
}

/**
 * The place of the shown card among `cards`: its own when it is still there,
 * else the place it had, which is the one its successor took, clamped to the
 * last card.
 */
export function cardIndex(
  cards: readonly TrayCard[],
  shownId: string | null,
  lastIndex: number,
): number {
  if (cards.length === 0) {
    return 0;
  }
  if (shownId !== null) {
    const at = cards.findIndex((card) => card.sessionId === shownId);
    if (at !== -1) {
      return at;
    }
  }
  return clampCard(lastIndex, cards.length);
}

/**
 * The actions state after a tap that is not the Stop waiting for its second
 * one: that Stop is dropped, so its row reads Stop again.
 */
export function tappedElsewhere(actions: HomeActions): HomeActions {
  return disarm(actions);
}

/** The actions state after a session's own update: an armed Stop on it is dropped. */
export function updatedSession(actions: HomeActions, sessionId: string): HomeActions {
  return actions.armed === sessionId ? disarm(actions) : actions;
}

/** What every group of one screen is drawn from. */
interface GroupContext {
  readonly model: HomeModel;
  readonly actions: HomeActions;
  readonly presets: HomePresets;
  readonly actionsEnabled: boolean;
  readonly supportsLaunchLast: boolean;
}

/** Home for one model, actions state, connection state and presets state. */
export function homeScreen(
  model: HomeModel,
  actions: HomeActions,
  state: HomeState,
  presets: HomePresets = NO_PRESETS,
): HomeScreen {
  const bar = homeBar(state);
  const actionsEnabled = bar !== RECONNECTING_BAR;
  const cards = needsYou(model);
  const at = cardIndex(cards, state.card, state.index);
  const shown = cards[at];
  const supportsLaunchLast = state.supportsLaunchLast;
  const context: GroupContext = { model, actions, presets, actionsEnabled, supportsLaunchLast };
  const groups = homeGroups(model)
    .filter((group) => supportsLaunchLast || group.kind !== "launch_again")
    .map((group) => groupView(group, context));
  return {
    visible: homeVisible(state.status, state.shown, state.paired),
    bar,
    actionsEnabled,
    index: at,
    card: shown === undefined ? null : cardView(shown, cards, at, actions, actionsEnabled),
    groups,
    // Before the first list the model is empty too, so only a loaded one says
    // so; a Launch again group the host cannot show is no content either.
    empty: model.loaded && groups.length === 0 && shown === undefined ? NO_SESSIONS : null,
    newSession: newSessionView(model, actions, state, actionsEnabled),
  };
}

/** The New session button and its open list; null before the list loads or on a host without it. */
function newSessionView(
  model: HomeModel,
  actions: HomeActions,
  state: HomeState,
  actionsEnabled: boolean,
): NewSessionView | null {
  if (!model.loaded) {
    return null;
  }
  if (!state.supportsNewSession) {
    return droppedNewSessions(model, actions);
  }
  const open = state.newSessionOpen;
  return {
    label: NEW_SESSION,
    open,
    enabled: actionsEnabled,
    rows: open
      ? newSessionChoices(model).map((choice) => newSessionRow(choice, actions, actionsEnabled))
      : [],
  };
}

/**
 * On a host without `new_session`: no button, only the containers whose New
 * session was dropped, each with why; null when there are none.
 */
function droppedNewSessions(model: HomeModel, actions: HomeActions): NewSessionView | null {
  if (actions.newSessionErrors.size === 0) {
    return null;
  }
  const rows = newSessionChoices(model).flatMap((choice) => {
    const error = actions.newSessionErrors.get(containerKey(choice.target));
    return error === undefined
      ? []
      : [{ target: choice.target, name: choice.name, label: null, enabled: false, error }];
  });
  return rows.length === 0 ? null : { label: null, open: true, enabled: false, rows };
}

/** One picker row: `Starting…` while its own New session waits, off while any launch on it waits. */
function newSessionRow(
  choice: NewSessionChoice,
  actions: HomeActions,
  actionsEnabled: boolean,
): NewSessionRowView {
  const pending = pendingLaunchKind(actions, choice.target);
  return {
    target: choice.target,
    name: choice.name,
    label: pending === "new_session" ? STARTING : START,
    enabled: actionsEnabled && pending === null,
    error: actions.newSessionErrors.get(containerKey(choice.target)) ?? null,
  };
}

/** Whether two screens are equal, so painting one over the other would show the same. */
export function sameScreen(left: HomeScreen, right: HomeScreen): boolean {
  return sameValue(left, right);
}

/**
 * The status word a row shows, from the label the daemon computed; a daemon
 * that sent no label, or one this build does not know, words the status.
 */
export function statusWord(session: SessionSnapshot): string {
  switch (session.status_label) {
    case "inactive":
      return "Inactive";
    case "abandoned":
      return "Abandoned";
    case "orphan":
      return "Orphaned";
    case "starting":
      return "Starting";
    case "awaiting_input":
      return "Needs you";
    case "working":
      return "Working";
    case "background":
      return "Working in the background";
    case "idle":
      return "Idle";
    case "error":
      return "Error";
    case "stopped":
      return "Stopped";
    default:
      return statusOnlyWord(session.status);
  }
}

/** The word for a status alone; an unknown status shows as the daemon sent it. */
function statusOnlyWord(status: SessionStatus): string {
  switch (status) {
    case "working":
      return "Working";
    case "awaiting_input":
      return "Needs you";
    case "idle":
      return "Idle";
    case "stopped":
      return "Stopped";
    case "error":
      return "Error";
    case "spawning":
      return "Starting";
    default:
      return status;
  }
}

function cardView(
  card: TrayCard,
  cards: readonly TrayCard[],
  at: number,
  actions: HomeActions,
  actionsEnabled: boolean,
): CardView {
  const sending = pendingAnswer(actions, card.sessionId);
  return {
    sessionId: card.sessionId,
    label: card.label,
    detail: card.detail,
    since: card.since,
    position: `${at + 1} of ${cards.length}`,
    count: cards.length,
    buttons: card.choices.map((choice) => ({
      label: choice.label,
      hint: choice.hint,
      choice: choice.choice,
      enabled: actionsEnabled && !sending,
    })),
    open: card.open,
    openEnabled: actionsEnabled,
    error: errorFor(actions, card.sessionId),
    sending: sending ? SENDING : null,
    previous: actionsEnabled && at > 0 ? (cards[at - 1]?.sessionId ?? null) : null,
    next: actionsEnabled && at < cards.length - 1 ? (cards[at + 1]?.sessionId ?? null) : null,
  };
}

function groupView(group: HomeGroup, context: GroupContext): GroupView {
  const { actions, actionsEnabled, supportsLaunchLast } = context;
  return {
    name: group.name,
    rows: group.rows.map((session) => rowView(session, actions, actionsEnabled)),
    presets: presetsView(group, context),
    launch: launchView(group.launch, actions, actionsEnabled, supportsLaunchLast),
    launchError: group.launch === null ? null : launchErrorFor(actions, group.launch),
    launches: (group.containers ?? []).map((container) => ({
      name: container.name,
      launch: launchView(container.launch, actions, actionsEnabled, supportsLaunchLast),
      launchError: launchErrorFor(actions, container.launch),
    })),
  };
}

/** A repo's or workspace's presets toggle and its list; null for any other group. */
function presetsView(group: HomeGroup, context: GroupContext): PresetsView | null {
  if (group.kind !== "repo" && group.kind !== "workspace") {
    return null;
  }
  const target: ContainerRef = { kind: group.kind, id: group.id };
  const key = containerKey(target);
  const open = context.presets.open.has(key);
  const list = open ? context.presets.lists.get(key) : undefined;
  return {
    target,
    label: PRESETS,
    open,
    enabled: context.actionsEnabled,
    error: list?.kind === "refused" ? list.error : null,
    rows:
      list?.kind === "listed" ? list.entries.map((entry) => presetRow(entry, target, context)) : [],
  };
}

/** One preset's row: its job's progress while that runs, else its button and its last outcome. */
function presetRow(entry: PresetEntry, target: ContainerRef, context: GroupContext): PresetRowView {
  const job = jobFor(context.presets, containerKey(target), entry.id);
  const base = { presetId: entry.id, name: entry.name };
  if (job !== undefined && isRunning(job)) {
    const progress = `Launching ${job.launched}/${job.total}`;
    return { ...base, launch: null, progress, cancel: job.jobId, note: null };
  }
  const key = presetKey(target, entry.id);
  const source = phoneLaunch(entry, context.model.repos, context.presets.agents);
  const pending = context.actions.presetLaunches.some(
    (launch) => presetKey(launch.target, launch.presetId) === key,
  );
  const launch: PresetLaunchView =
    source === null
      ? { label: NEEDS_DESKTOP, enabled: false, source: null }
      : { label: LAUNCH_PRESET, enabled: context.actionsEnabled && !pending, source };
  // A job that has answered is newer than any dropped-launch note: its outcome wins.
  const note =
    job !== undefined && job.status !== null
      ? jobNote(job)
      : (context.actions.presetErrors.get(key) ?? null);
  return { ...base, launch, progress: null, cancel: null, note };
}

/** A job that has answered and not ended: its progress shows, an unknown status's too. */
function isRunning(job: PresetJob): boolean {
  return job.status !== null && !jobEnded(job);
}

/** What an ended job leaves on its row: a failure's error, `Cancelled`, the lost-track text, or nothing. */
function jobNote(job: PresetJob): string | null {
  switch (job.status) {
    case "failed":
      return job.error;
    case "cancelled":
      return CANCELLED;
    case LOST_STATUS:
      return LOST_JOB;
    default:
      return null;
  }
}

/** The Launch last button a container offers; null when the host cannot launch. */
function launchView(
  target: ContainerRef | null,
  actions: HomeActions,
  actionsEnabled: boolean,
  supportsLaunchLast: boolean,
): LaunchView | null {
  if (target === null || !supportsLaunchLast) {
    return null;
  }
  // Either kind of launch waiting on the container disables it; only its own says so.
  const pending = pendingLaunchKind(actions, target);
  return {
    target,
    label: pending === "launch_last" ? LAUNCHING : LAUNCH_LAST,
    enabled: actionsEnabled && pending === null,
  };
}

function launchErrorFor(actions: HomeActions, target: ContainerRef): string | null {
  return actions.launchErrors.get(containerKey(target)) ?? null;
}

function rowView(session: SessionSnapshot, actions: HomeActions, actionsEnabled: boolean): RowView {
  const stop = stopOffered(session);
  return {
    sessionId: session.id,
    label: displayLabel(session),
    status: statusWord(session),
    stop: stop ? (actions.armed === session.id ? STOP_ARMED : STOP) : null,
    resume: resumeOffered(session),
    enabled: actionsEnabled,
    error: errorFor(actions, session.id),
  };
}

/** Whether the session has an answer on its way to the daemon. */
function pendingAnswer(actions: HomeActions, sessionId: string): boolean {
  return actions.pending.some(
    (request) => request.sessionId === sessionId && request.action === "answer",
  );
}

/** Stop is offered while the session is up, as the desktop's menu offers it. */
function stopOffered(session: SessionSnapshot): boolean {
  return LIVE.has(session.status) && session.is_inactive !== true && session.is_abandoned !== true;
}

/**
 * Resume is offered exactly where `home-actions`' `resume` sends: never for a
 * headless session, and for an abandoned, parked, stopped or errored one.
 */
function resumeOffered(session: SessionSnapshot): boolean {
  if (session.mode === "headless") {
    return false;
  }
  return (
    session.is_abandoned === true ||
    session.is_inactive === true ||
    session.status === "stopped" ||
    session.status === "error"
  );
}

/** The name a row shows, as the tray's card shows it: the user label when it has one. */
export function displayLabel(session: SessionSnapshot): string {
  const own = session.user_label?.trim() ?? "";
  return own === "" ? session.label : own;
}

/** Structural equality over the plain data a screen is made of. */
function sameValue(left: unknown, right: unknown): boolean {
  if (left === right) {
    return true;
  }
  if (Array.isArray(left) || Array.isArray(right)) {
    if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
      return false;
    }
    return left.every((item, at) => sameValue(item, right[at]));
  }
  if (typeof left !== "object" || typeof right !== "object" || left === null || right === null) {
    return false;
  }
  const a = left as Readonly<Record<string, unknown>>;
  const b = right as Readonly<Record<string, unknown>>;
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) {
    return false;
  }
  return keys.every((key) => key in b && sameValue(a[key], b[key]));
}
