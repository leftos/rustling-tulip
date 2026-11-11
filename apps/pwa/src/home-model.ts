import type {
  ContainerRef,
  DaemonMessage,
  PendingInput,
  PendingQuestion,
  PromptChoice,
  RepoEntry,
  SessionSnapshot,
  SpawnConfig,
  WorkspaceEntry,
} from "./protocol";

/** What a group of Home rows is: the desktop sidebar's container kinds, and the launches waiting for a session. */
export type HomeGroupKind = "workspace" | "repo" | "shell" | "folder" | "detached" | "launch_again";

/** What the trailing group of containers with nothing running is called. */
const LAUNCH_AGAIN = "Launch again";

/**
 * The phone's Home state, folded from the daemon's repo, workspace and
 * session messages. The first `sessions` list after a reset is sorted by
 * label, case-insensitive, then id, as the desktop sidebar sorts; a session
 * named after it, by an update or a later list, goes last. A row then keeps
 * its place whenever its state changes; it moves only when a container's
 * manual order says so.
 */
export interface HomeModel {
  readonly repos: readonly RepoEntry[];
  readonly workspaces: readonly WorkspaceEntry[];
  /** Every known session, in the order the server first named it. */
  readonly sessions: readonly SessionSnapshot[];
  /** Manual orders from `sessions_reordered`, by container id; newest wins. */
  readonly orders: ReadonlyMap<string, readonly string[]>;
  /** The user's manual container order, workspaces and repos mixed. */
  readonly containerOrder: readonly ContainerRef[];
  /**
   * Whether the daemon's first `sessions` message has arrived, so the list is
   * the whole truth about which sessions exist.
   */
  readonly loaded: boolean;
}

/** The model before any message: what `reset` returns. */
export const INITIAL_HOME: HomeModel = {
  repos: [],
  workspaces: [],
  sessions: [],
  orders: new Map(),
  containerOrder: [],
  loaded: false,
};

/**
 * The model a screen starts from on every `ready`: the daemon resends its
 * whole initial state after a reconnect, so nothing from the old connection
 * is kept.
 */
export function reset(): HomeModel {
  return INITIAL_HOME;
}

/**
 * One daemon message folded into the model. Pure; a message the model does
 * not read returns the model itself, so a caller can tell by identity.
 */
export function homeStep(model: HomeModel, message: DaemonMessage): HomeModel {
  switch (message.type) {
    case "repos":
      return { ...model, repos: message.repos };
    case "workspaces":
      return { ...model, workspaces: message.workspaces };
    case "sessions":
      return {
        ...model,
        sessions: model.loaded
          ? keptInPlace(model.sessions, message.sessions)
          : byLabel(message.sessions),
        loaded: true,
      };
    case "session_updated":
      return { ...model, sessions: withSession(model.sessions, message.session) };
    case "session_removed": {
      const sessions = model.sessions.filter((session) => session.id !== message.session_id);
      return sessions.length === model.sessions.length ? model : { ...model, sessions };
    }
    case "sessions_reordered": {
      // The phone draws one "Shells" group and one "Folders" group, so an
      // order for a desktop shell or folder container has nothing here to
      // apply; a workspace's or a repo's order does.
      const orders = new Map(model.orders);
      orders.set(message.container_id, message.ordered_ids);
      return { ...model, orders };
    }
    case "containers_reordered":
      return { ...model, containerOrder: message.ordered };
    default:
      return model;
  }
}

/**
 * The daemon's first list sorted as the desktop sidebar sorts: by the label a
 * row shows, case-insensitive, then by id. The daemon's own order is not
 * stable across a relaunch.
 */
function byLabel(sessions: readonly SessionSnapshot[]): readonly SessionSnapshot[] {
  return sessions
    .slice()
    .sort((a, b) => compareNames(displayLabel(a), displayLabel(b)) || compareIds(a.id, b.id));
}

/**
 * A later full list (a resync, a re-listing) over the rows already drawn: a
 * known session keeps its place with its new state, one the list no longer
 * names leaves, and a new one goes last in the list's order.
 */
function keptInPlace(
  known: readonly SessionSnapshot[],
  listed: readonly SessionSnapshot[],
): readonly SessionSnapshot[] {
  const byId = new Map(listed.map((session) => [session.id, session]));
  const kept = known.flatMap((session) => byId.get(session.id) ?? []);
  const knownIds = new Set(known.map((session) => session.id));
  return [...kept, ...listed.filter((session) => !knownIds.has(session.id))];
}

/** The plain order of two session ids. */
function compareIds(a: string, b: string): number {
  if (a < b) {
    return -1;
  }
  return a > b ? 1 : 0;
}

/** `sessions` with `session` replacing its own row in place, or appended. */
function withSession(
  sessions: readonly SessionSnapshot[],
  session: SessionSnapshot,
): readonly SessionSnapshot[] {
  const at = sessions.findIndex((known) => known.id === session.id);
  if (at === -1) {
    return [...sessions, session];
  }
  const next = sessions.slice();
  next[at] = session;
  return next;
}

/** One container a group offers to launch again, for the trailing group. */
export interface LaunchContainer {
  readonly name: string;
  readonly launch: ContainerRef;
}

/** One group of rows: a workspace, a repo, or one of the single buckets. */
export interface HomeGroup {
  readonly kind: HomeGroupKind;
  /** The container's id on the wire: a workspace's, a repo's, else the kind. */
  readonly id: string;
  readonly name: string;
  readonly rows: readonly SessionSnapshot[];
  /** The container's stored last launch; null when it has none to replay. */
  readonly launch: ContainerRef | null;
  /** The containers the Launch again group lists; absent for every other group. */
  readonly containers?: readonly LaunchContainer[];
}

/** The rows grouped as the desktop's sidebar groups them, in its order. */
export function homeGroups(model: HomeModel): readonly HomeGroup[] {
  const members = memberRepoIds(model);
  const build: GroupBuild = {
    model,
    bucketed: bucketSessions(model, members),
    workspaces: new Map(model.workspaces.map((workspace) => [workspace.id, workspace])),
    repos: new Map(
      model.repos.filter((repo) => !members.has(repo.id)).map((repo) => [repo.id, repo]),
    ),
    groups: [],
    trailing: [],
  };
  const walk: ContainerWalk = {
    model,
    workspaces: build.workspaces,
    repos: build.repos,
    workspace: (workspace) => pushWorkspaceGroup(build, workspace),
    repo: (repo) => pushRepoGroup(build, repo),
  };
  pushRemaining(walk, pushManualOrder(walk));
  pushGroup(build.groups, "shell", "shell", "Shells", build.bucketed.shells);
  pushGroup(build.groups, "folder", "folder", "Folders", build.bucketed.folders);
  pushGroup(build.groups, "detached", "detached", "Detached", build.bucketed.detached);
  const again = launchAgainGroup(build.trailing);
  if (again !== null) {
    build.groups.push(again);
  }
  return build.groups;
}

/** What one group list is built from, passed along to the helpers that fill it. */
interface GroupBuild {
  readonly model: HomeModel;
  readonly bucketed: Buckets;
  readonly workspaces: ReadonlyMap<string, WorkspaceEntry>;
  readonly repos: ReadonlyMap<string, RepoEntry>;
  readonly groups: HomeGroup[];
  readonly trailing: LaunchContainer[];
}

/** One container the New session picker offers: its name and where the session starts. */
export interface NewSessionChoice {
  readonly name: string;
  readonly target: ContainerRef;
}

/**
 * The containers a New session can start in, in Home's container order:
 * every workspace and every repo that is no workspace's member, git or not,
 * whether or not it has launched or holds a session. A member repo's
 * sessions belong to its workspace.
 */
export function newSessionChoices(model: HomeModel): readonly NewSessionChoice[] {
  const members = memberRepoIds(model);
  const choices: NewSessionChoice[] = [];
  const walk: ContainerWalk = {
    model,
    workspaces: new Map(model.workspaces.map((workspace) => [workspace.id, workspace])),
    repos: new Map(
      model.repos.filter((repo) => !members.has(repo.id)).map((repo) => [repo.id, repo]),
    ),
    workspace: (workspace) =>
      choices.push({ name: workspace.name, target: { kind: "workspace", id: workspace.id } }),
    repo: (repo) => choices.push({ name: repo.name, target: { kind: "repo", id: repo.id } }),
  };
  pushRemaining(walk, pushManualOrder(walk));
  return choices;
}

/** The containers to walk in Home's order, and what each visit does with one. */
interface ContainerWalk {
  readonly model: HomeModel;
  readonly workspaces: ReadonlyMap<string, WorkspaceEntry>;
  readonly repos: ReadonlyMap<string, RepoEntry>;
  readonly workspace: (workspace: WorkspaceEntry) => void;
  readonly repo: (repo: RepoEntry) => void;
}

/** Every repo a workspace holds, so its sessions stay in the workspace's group. */
function memberRepoIds(model: HomeModel): Set<string> {
  const members = new Set<string>();
  for (const workspace of model.workspaces) {
    for (const id of workspace.member_repo_ids) {
      members.add(id);
    }
  }
  return members;
}

/** Every session filed where the desktop's `build_containers` files it. */
function bucketSessions(model: HomeModel, members: ReadonlySet<string>): Buckets {
  const bucketed: Buckets = {
    workspaces: new Map(),
    repos: new Map(),
    shells: [],
    folders: [],
    detached: [],
  };
  for (const session of model.sessions) {
    place(session, model, members, bucketed);
  }
  return bucketed;
}

/**
 * The user's manual order first, workspaces and repos mixed, the desktop's
 * first pass in `apply_container_order`; returns every container it placed.
 */
function pushManualOrder(walk: ContainerWalk): Set<string> {
  const placed = new Set<string>();
  for (const ref of walk.model.containerOrder) {
    const key = `${ref.kind}:${ref.id}`;
    if (placed.has(key)) {
      continue;
    }
    if (ref.kind === "workspace") {
      const workspace = walk.workspaces.get(ref.id);
      if (workspace !== undefined) {
        placed.add(key);
        walk.workspace(workspace);
      }
    } else {
      const repo = walk.repos.get(ref.id);
      if (repo !== undefined) {
        placed.add(key);
        walk.repo(repo);
      }
    }
  }
  return placed;
}

/** The containers the manual order did not name, alphabetically, the desktop's second pass. */
function pushRemaining(walk: ContainerWalk, placed: ReadonlySet<string>): void {
  const workspaces = [...walk.workspaces.values()].sort((a, b) => compareNames(a.name, b.name));
  for (const workspace of workspaces) {
    if (!placed.has(`workspace:${workspace.id}`)) {
      walk.workspace(workspace);
    }
  }
  const repos = [...walk.repos.values()].sort((a, b) => compareNames(a.name, b.name));
  for (const repo of repos) {
    if (!placed.has(`repo:${repo.id}`)) {
      walk.repo(repo);
    }
  }
}

/** The trailing group of containers with nothing running; null when there are none. */
function launchAgainGroup(trailing: readonly LaunchContainer[]): HomeGroup | null {
  if (trailing.length === 0) {
    return null;
  }
  return {
    kind: "launch_again",
    id: "launch_again",
    name: LAUNCH_AGAIN,
    rows: [],
    launch: null,
    containers: trailing,
  };
}

/** Where a group's rows are collected before the groups are built. */
interface Buckets {
  readonly workspaces: Map<string, SessionSnapshot[]>;
  readonly repos: Map<string, SessionSnapshot[]>;
  readonly shells: SessionSnapshot[];
  readonly folders: SessionSnapshot[];
  readonly detached: SessionSnapshot[];
}

/** A group with the rows it holds; a bucket holding none is left out. */
function pushGroup(
  groups: HomeGroup[],
  kind: HomeGroupKind,
  id: string,
  name: string,
  rows: readonly SessionSnapshot[],
): void {
  if (rows.length > 0) {
    groups.push({ kind, id, name, rows, launch: null });
  }
}

/** One workspace or repo group before the trailing group is decided. */
interface ContainerGroup {
  readonly kind: "workspace" | "repo";
  readonly id: string;
  readonly name: string;
  readonly rows: readonly SessionSnapshot[];
  readonly launch: ContainerRef | null;
}

/**
 * A workspace or repo group, or, when it has no rows, its place in the
 * trailing Launch again group if it has a launch to offer.
 */
function pushContainer(
  groups: HomeGroup[],
  trailing: LaunchContainer[],
  entry: ContainerGroup,
): void {
  if (entry.rows.length > 0) {
    groups.push({
      kind: entry.kind,
      id: entry.id,
      name: entry.name,
      rows: entry.rows,
      launch: entry.launch,
    });
  } else if (entry.launch !== null) {
    trailing.push({ name: entry.name, launch: entry.launch });
  }
}

function pushWorkspaceGroup(build: GroupBuild, workspace: WorkspaceEntry): void {
  pushContainer(build.groups, build.trailing, {
    kind: "workspace",
    id: workspace.id,
    name: workspace.name,
    rows: ordered(
      build.bucketed.workspaces.get(workspace.id) ?? [],
      build.model.orders.get(workspace.id),
    ),
    launch: containerLaunch("workspace", workspace.id, workspace.last_spawn_config),
  });
}

function pushRepoGroup(build: GroupBuild, repo: RepoEntry): void {
  pushContainer(build.groups, build.trailing, {
    kind: "repo",
    id: repo.id,
    name: repo.name,
    rows: ordered(build.bucketed.repos.get(repo.id) ?? [], build.model.orders.get(repo.id)),
    launch: containerLaunch("repo", repo.id, repo.last_spawn_config),
  });
}

/** The launch a container's stored config offers; a missing or headless one offers none. */
function containerLaunch(
  kind: "repo" | "workspace",
  id: string,
  config: SpawnConfig | null | undefined,
): ContainerRef | null {
  if (config === null || config === undefined || config.mode === "headless") {
    return null;
  }
  return { kind, id };
}

/** Rows reordered by `order`; a row it does not name keeps its first-seen place after the named ones. */
function ordered(
  rows: readonly SessionSnapshot[],
  order: readonly string[] | undefined,
): readonly SessionSnapshot[] {
  if (order === undefined) {
    return rows;
  }
  const rank = new Map<string, number>();
  order.forEach((id, at) => {
    if (!rank.has(id)) {
      rank.set(id, at);
    }
  });
  const last = order.length;
  return rows.slice().sort((a, b) => (rank.get(a.id) ?? last) - (rank.get(b.id) ?? last));
}

/** Files `session` where the desktop's `build_containers` files it. */
function place(
  session: SessionSnapshot,
  model: HomeModel,
  members: ReadonlySet<string>,
  buckets: Buckets,
): void {
  const cwd = session.current_cwd ?? "";
  if (session.mode === "plain_shell" && cwd !== "") {
    placeByCwd(session, cwd, model, members, buckets);
  } else if (session.kind === "standalone") {
    buckets.shells.push(session);
  } else {
    placeByOwner(session, model, members, buckets);
  }
}

/** A shell in a known folder files under its workspace, repo or folder. */
function placeByCwd(
  session: SessionSnapshot,
  cwd: string,
  model: HomeModel,
  members: ReadonlySet<string>,
  buckets: Buckets,
): void {
  const repo = bestRepoFor(cwd, model.repos);
  if (repo !== undefined) {
    const workspace = model.workspaces.find((w) => w.member_repo_ids.includes(repo.id));
    if (workspace !== undefined) {
      push(buckets.workspaces, workspace.id, session);
      return;
    }
    if (!members.has(repo.id)) {
      push(buckets.repos, repo.id, session);
      return;
    }
  }
  if (session.kind === "standalone") {
    buckets.shells.push(session);
  } else {
    buckets.folders.push(session);
  }
}

/** The repo whose folder holds `cwd`, the deepest one when several do. */
function bestRepoFor(cwd: string, repos: readonly RepoEntry[]): RepoEntry | undefined {
  let best: RepoEntry | undefined;
  for (const repo of repos) {
    if (
      pathContains(repo.path, cwd) &&
      (best === undefined || repo.path.length > best.path.length)
    ) {
      best = repo;
    }
  }
  return best;
}

/** Whether the folder `parent` holds the folder `child`, either spelling of the separator. */
function pathContains(parent: string, child: string): boolean {
  const above = slash(parent).toLowerCase();
  const below = slash(child).toLowerCase();
  if (above === below) {
    return true;
  }
  return below.startsWith(`${above}/`);
}

function slash(path: string): string {
  return path.replaceAll("\\", "/").replace(/\/+$/, "");
}

/** A session with no workspace of its own files under its repo, else Detached. */
function placeByOwner(
  session: SessionSnapshot,
  model: HomeModel,
  members: ReadonlySet<string>,
  buckets: Buckets,
): void {
  const workspaceId = session.workspace_id ?? null;
  if (workspaceId !== null) {
    if (model.workspaces.some((w) => w.id === workspaceId)) {
      push(buckets.workspaces, workspaceId, session);
    } else {
      buckets.detached.push(session);
    }
    return;
  }
  // The owner is the first member, as the desktop reads it: a session whose
  // first member is a workspace member has no repo of its own and is Detached.
  const owner = session.members[0]?.repo_id;
  if (owner !== undefined && !members.has(owner) && model.repos.some((r) => r.id === owner)) {
    push(buckets.repos, owner, session);
  } else {
    buckets.detached.push(session);
  }
}

function push(bucket: Map<string, SessionSnapshot[]>, id: string, session: SessionSnapshot): void {
  const rows = bucket.get(id);
  if (rows === undefined) {
    bucket.set(id, [session]);
  } else {
    rows.push(session);
  }
}

/** Case-insensitive order of two names, as the desktop sorts containers. */
function compareNames(a: string, b: string): number {
  const left = a.toLowerCase();
  const right = b.toLowerCase();
  if (left < right) {
    return -1;
  }
  return left > right ? 1 : 0;
}

/** One button of a Needs you card. */
export interface TrayChoice {
  readonly label: string;
  /** The tray's key hint, `1` to `n`; null when the choice shows none. */
  readonly hint: string | null;
  readonly choice: PromptChoice;
}

/**
 * One Needs you card: the session, what it asks, and the answers the tray
 * offers. A card with no choices carries the Open action instead, since
 * anything longer than a yes/no or a permission question opens the session.
 */
export interface TrayCard {
  readonly sessionId: string;
  readonly label: string;
  readonly detail: string;
  /** The `status_since` an answer is sent against; null without one. */
  readonly since: string | null;
  readonly choices: readonly TrayChoice[];
  /** The label of the button that opens the session; null when the card answers. */
  readonly open: "Open" | null;
}

/** The card's line when nothing describes what the session waits on. */
export const WAITING_FOR_YOU = "Waiting for you";

/** The card's line for a plan-approval prompt. */
export const PLAN_READY = "Plan ready for review";

/** The card's action for a prompt the tray cannot answer. */
const OPEN = "Open";

/**
 * The cards the Needs you tray holds, one per session waiting for input:
 * the longest wait first, a session with no stamp after those. Parked and
 * abandoned sessions are never listed.
 */
export function needsYou(model: HomeModel): readonly TrayCard[] {
  return model.sessions
    .filter(
      (session) =>
        session.status === "awaiting_input" && !session.is_inactive && !session.is_abandoned,
    )
    .slice()
    .sort((a, b) => {
      const left = waitStarted(a);
      const right = waitStarted(b);
      if (left === null || right === null) {
        return left === right ? 0 : left === null ? 1 : -1;
      }
      return left - right;
    })
    .map(card);
}

/** How many sessions need the user; what the badge shows. */
export function needsYouCount(model: HomeModel): number {
  return needsYou(model).length;
}

/** When the session's wait started, in ms; null when it carries no stamp. */
function waitStarted(session: SessionSnapshot): number | null {
  const since = session.status_since ?? null;
  if (since === null) {
    return null;
  }
  const at = Date.parse(since);
  return Number.isNaN(at) ? null : at;
}

function card(session: SessionSnapshot): TrayCard {
  const prompt = session.pending_input ?? null;
  const choices = choicesOf(prompt, session.status_since ?? null);
  return {
    sessionId: session.id,
    label: displayLabel(session),
    detail: detailOf(prompt),
    since: session.status_since ?? null,
    choices,
    open: choices.length === 0 ? OPEN : null,
  };
}

/** The name a row and its card show. */
function displayLabel(session: SessionSnapshot): string {
  const own = session.user_label?.trim() ?? "";
  return own === "" ? session.label : own;
}

/** The answers a prompt offers, and none for one that opens the session. */
function choicesOf(prompt: PendingInput | null, since: string | null): readonly TrayChoice[] {
  if (since === null) {
    return [];
  }
  switch (prompt?.kind) {
    case "question":
      return questionChoices(prompt.questions);
    case "permission":
      return [
        { label: "Allow", hint: null, choice: { kind: "allow" } },
        { label: "Deny", hint: null, choice: { kind: "deny" } },
      ];
    case "plan_approval":
    case "other":
    case undefined:
      return [];
    default:
      return [];
  }
}

/** One button per option of a single single-choice question, hinted `1` to `n`. */
function questionChoices(questions: readonly PendingQuestion[]): readonly TrayChoice[] {
  if (questions.length !== 1) {
    return [];
  }
  const question = questions[0];
  if (question === undefined || question.multi_select === true || question.preview === true) {
    return [];
  }
  const options = question.options ?? [];
  return options.map((label, index) => ({
    label,
    hint: String(index + 1),
    choice: { kind: "option", index },
  }));
}

/** What the card says the session waits on. */
function detailOf(prompt: PendingInput | null): string {
  switch (prompt?.kind) {
    case "question": {
      const text = prompt.questions[0]?.question.trim() ?? "";
      return text === "" ? WAITING_FOR_YOU : text;
    }
    case "permission": {
      const tool = prompt.tool_name.trim();
      return tool === "" ? WAITING_FOR_YOU : `Allow ${tool}?`;
    }
    case "plan_approval":
      return PLAN_READY;
    case "other": {
      const text = prompt.message.trim();
      return text === "" ? WAITING_FOR_YOU : prompt.message;
    }
    default:
      return WAITING_FOR_YOU;
  }
}
