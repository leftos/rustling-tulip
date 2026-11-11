import {
  answer,
  expireLaunches,
  expirePresetLaunches,
  foldReply,
  forgetPresetError,
  keepLaunches,
  launchLast,
  launchPreset,
  reset as resetActions,
  resendLaunches,
  resendPresetLaunches,
  resume as requestResume,
  stop as requestStop,
  type HomeActions,
  type PresetLaunchRequest,
} from "./home-actions";
import { homeStep, reset as resetModel, type HomeModel } from "./home-model";
import {
  NO_PRESETS,
  answeredPresetKey,
  foldPresets,
  reconnectPresets,
  togglePresets,
  trackJob,
  type HomePresets,
} from "./home-presets";
import { homeNodes, type HomeHandlers } from "./home-render";
import {
  homeScreen,
  sameScreen,
  tappedElsewhere,
  updatedSession,
  type CardButton,
  type CardView,
  type HomeScreen,
  type HomeState,
} from "./home-screen";
import type { ConnectedLatch } from "./connection-bar";
import type { ConnectionStateName } from "./connection-machine";
import { introLineShown } from "./installed";
import {
  FEATURE_LAUNCH_LAST,
  FEATURE_PRESET_JOB_REPEAT,
  FEATURE_RESUME_SESSION,
  type ClientMessage,
  type ContainerRef,
  type DaemonMessage,
} from "./protocol";
import type { Connection } from "./socket";

/** The page's intro header, its install line, and whether the page runs from the Home Screen. */
export interface Intro {
  readonly header: HTMLElement;
  readonly line: HTMLElement | null;
  readonly installed: boolean;
}

/** How often Home checks whether an unanswered launch has waited too long. */
const LAUNCH_EXPIRY_TICK_MS = 5_000;

/** What the mounted view keeps between notices. */
interface HomeView {
  model: HomeModel;
  actions: HomeActions;
  presets: HomePresets;
  status: ConnectionStateName;
  cardId: string | null;
  lastIndex: number;
  offline: boolean;
  lastSupportsLaunchLast: boolean;
  drawn: HomeScreen | null;
  expiry: ReturnType<typeof setInterval> | null;
}

/** What the view was mounted with: the page's nodes and the calls back into it. */
interface HomeContext {
  readonly container: HTMLElement;
  readonly connection: Connection;
  readonly intro: Intro | null;
  readonly onOpen: (sessionId: string) => void;
  readonly onModel: (model: HomeModel) => void;
  readonly connectedOnce: ConnectedLatch;
}

/** What the paint half offers the action half: a request id, the send, and a repaint. */
interface HomePaint {
  newRequestId(): string;
  send(message: ClientMessage): void;
  update(handlers: HomeHandlers): void;
}

/**
 * Home over one connection: it folds the notices into the model and the
 * actions state, keeps the connection's status and the tray's shown card,
 * paints the [`HomeScreen`] the model makes when it differs from the last one
 * drawn, and wires the tray's answers and moves and the rows' Stop, Resume and
 * tap to the action functions. It holds no logic beyond that; the texts all
 * come from the model. A row's tap and the tray's Open hand their session to
 * `onOpen`, which navigates the page, and every change of the model is handed
 * to `onModel`, so a session's screen reads the one Home model.
 */
export function mountHomeView(
  container: HTMLElement,
  connection: Connection,
  intro: Intro | null,
  onOpen: (sessionId: string) => void,
  onModel: (model: HomeModel) => void,
  connectedOnce: ConnectedLatch,
): () => void {
  const context: HomeContext = { container, connection, intro, onOpen, onModel, connectedOnce };
  const view: HomeView = {
    model: resetModel(),
    actions: resetActions(),
    presets: NO_PRESETS,
    status: "idle",
    cardId: null,
    lastIndex: 0,
    offline: false,
    lastSupportsLaunchLast: false,
    drawn: null,
    expiry: null,
  };
  const paint = createHomePaint(context, view);
  const handlers = createHomeActions(context, view, paint);

  // The install line never changes in a page visit; Home hides the whole header anyway.
  if (intro !== null && intro.line !== null) {
    intro.line.hidden = !introLineShown(intro.installed);
  }

  const unsubscribe = connection.subscribe((notice) => {
    if (notice.kind === "status") {
      onStatus(context, view, paint, handlers, notice.state);
      return;
    }
    if (notice.kind === "message") {
      onMessage(view, paint, handlers, notice.message);
    }
  });
  paint.update(handlers);

  return () => {
    unsubscribe();
    if (view.expiry !== null) {
      clearInterval(view.expiry);
      view.expiry = null;
    }
  };
}

/** One connection status, folded into the view and painted. */
function onStatus(
  context: HomeContext,
  view: HomeView,
  paint: HomePaint,
  handlers: HomeHandlers,
  state: ConnectionStateName,
): void {
  view.status = state;
  if (view.status === "ready") {
    onReady(context, view, paint);
  }
  paint.update(handlers);
}

/**
 * Every `ready` is a fresh start: the daemon resends its whole state, so
 * nothing from the old connection is kept, save a launch still waiting for
 * its answer, which is resent with its id, and the open presets lists, which
 * are listed again.
 */
function onReady(context: HomeContext, view: HomeView, paint: HomePaint): void {
  view.model = resetModel();
  view.actions = keepLaunches(view.actions);
  view.lastSupportsLaunchLast = context.connection.supports(FEATURE_LAUNCH_LAST);
  view.cardId = null;
  view.lastIndex = 0;
  view.offline = false;
  const now = Date.now();
  const resent = resendLaunches(view.actions, view.lastSupportsLaunchLast, now);
  const supportsRepeat = context.connection.supports(FEATURE_PRESET_JOB_REPEAT);
  const presetsResent = resendPresetLaunches(resent.state, supportsRepeat, now);
  const relisted = reconnectPresets(view.presets, paint.newRequestId, supportsRepeat, now);
  view.actions = presetsResent.state;
  view.presets = relisted.state;
  for (const message of [...resent.messages, ...presetsResent.messages, ...relisted.messages]) {
    paint.send(message);
  }
}

/** One daemon message folded into the model and the actions state, then painted. */
function onMessage(
  view: HomeView,
  paint: HomePaint,
  handlers: HomeHandlers,
  message: DaemonMessage,
): void {
  view.model = homeStep(view.model, message);
  // An update of the phone's job is newer than its preset's dropped-launch note.
  view.actions = forgetPresetError(view.actions, answeredPresetKey(view.presets, message));
  view.presets = foldPresets(view.presets, message);
  const folded = foldReply(view.actions, message);
  view.actions = folded.state;
  if (message.type === "session_updated") {
    view.actions = updatedSession(view.actions, message.session.id);
  }
  for (const effect of folded.effects) {
    paint.send(effect);
  }
  paint.update(handlers);
}

/**
 * The model-to-DOM half of Home: it hands the action half a request id and the
 * send, and it turns the model, the actions state and the connection's state
 * into the painted screen, skipping a paint that would show the same thing.
 */
function createHomePaint(context: HomeContext, view: HomeView): HomePaint {
  function newRequestId(): string {
    return crypto.randomUUID();
  }

  function send(message: ClientMessage): void {
    if (!context.connection.send(message)) {
      view.offline = true;
    }
  }

  function syncExpiry(handlers: HomeHandlers): void {
    const waiting = view.actions.launches.length > 0 || view.actions.presetLaunches.length > 0;
    if (waiting && view.expiry === null) {
      view.expiry = setInterval(() => expire(handlers), LAUNCH_EXPIRY_TICK_MS);
    } else if (!waiting && view.expiry !== null) {
      clearInterval(view.expiry);
      view.expiry = null;
    }
  }

  function expire(handlers: HomeHandlers): void {
    const now = Date.now();
    const next = expirePresetLaunches(expireLaunches(view.actions, now), now);
    if (next === view.actions) {
      return;
    }
    view.actions = next;
    update(handlers);
  }

  function update(handlers: HomeHandlers): void {
    context.onModel(view.model);
    syncExpiry(handlers);
    const state: HomeState = {
      status: view.status,
      paired: context.connection.paired(),
      shown: context.connectedOnce.connected(),
      card: view.cardId,
      index: view.lastIndex,
      offline: view.offline,
      supportsLaunchLast: view.lastSupportsLaunchLast,
    };
    const screen = homeScreen(view.model, view.actions, state, view.presets);
    if (screen.visible) {
      context.connectedOnce.mark();
    }
    view.cardId = screen.card?.sessionId ?? null;
    view.lastIndex = screen.index;
    // A repaint rebuilds every button, so a tap lands on a node that is
    // already gone when the screen draws the same thing anyway.
    if (view.drawn !== null && sameScreen(view.drawn, screen)) {
      return;
    }
    view.drawn = screen;
    paint(screen, handlers);
  }

  function paint(screen: HomeScreen, handlers: HomeHandlers): void {
    context.container.hidden = !screen.visible;
    if (context.intro !== null) {
      context.intro.header.hidden = screen.visible;
    }
    context.container.replaceChildren(...homeNodes(screen, handlers));
  }

  return { newRequestId, send, update };
}

/** The action half of Home: what a tap on a painted node asks the daemon to do. */
function createHomeActions(context: HomeContext, view: HomeView, paint: HomePaint): HomeHandlers {
  const handlers: HomeHandlers = {
    choose(card: CardView, choice: CardButton): void {
      if (card.since === null) {
        return;
      }
      view.actions = tappedElsewhere(view.actions);
      const result = answer(
        view.actions,
        paint.newRequestId,
        card.sessionId,
        card.since,
        choice.choice,
      );
      view.actions = result.state;
      if (result.message !== null) {
        paint.send(result.message);
      }
      paint.update(handlers);
    },
    launch(target: ContainerRef): void {
      view.actions = tappedElsewhere(view.actions);
      const result = launchLast(view.actions, target, paint.newRequestId, Date.now());
      view.actions = result.state;
      if (result.message !== null) {
        paint.send(result.message);
      }
      paint.update(handlers);
    },
    ...createPresetActions(view, paint, () => handlers),
    stop(sessionId: string): void {
      const result = requestStop(view.actions, sessionId);
      view.actions = result.state;
      if (result.message !== null) {
        paint.send(result.message);
      }
      paint.update(handlers);
    },
    resume(sessionId: string): void {
      const session = view.model.sessions.find((known) => known.id === sessionId);
      if (session === undefined) {
        return;
      }
      view.actions = tappedElsewhere(view.actions);
      const supportsResumeSession = context.connection.supports(FEATURE_RESUME_SESSION);
      const result = requestResume(
        view.actions,
        paint.newRequestId,
        session,
        supportsResumeSession,
      );
      view.actions = result.state;
      if (result.message !== null) {
        paint.send(result.message);
      }
      paint.update(handlers);
    },
    move(sessionId: string | null): void {
      if (sessionId === null) {
        return;
      }
      view.actions = tappedElsewhere(view.actions);
      view.cardId = sessionId;
      paint.update(handlers);
    },
    open(sessionId: string): void {
      view.actions = tappedElsewhere(view.actions);
      context.onOpen(sessionId);
      paint.update(handlers);
    },
  };
  return handlers;
}

/** What a tap on a group's presets toggle or a preset row asks the daemon to do. */
type PresetHandlers = Pick<HomeHandlers, "togglePresets" | "launchPreset" | "cancelPreset">;

/** The preset half of Home's actions; `handlers` hands back the whole set for the repaint. */
function createPresetActions(
  view: HomeView,
  paint: HomePaint,
  handlers: () => HomeHandlers,
): PresetHandlers {
  return {
    togglePresets(target: ContainerRef): void {
      view.actions = tappedElsewhere(view.actions);
      const result = togglePresets(view.presets, target, paint.newRequestId);
      view.presets = result.state;
      if (result.message !== null) {
        paint.send(result.message);
      }
      paint.update(handlers());
    },
    launchPreset(request: PresetLaunchRequest): void {
      view.actions = tappedElsewhere(view.actions);
      const result = launchPreset(view.actions, request, paint.newRequestId, Date.now());
      view.actions = result.state;
      // A launch that went out is the newest pending record: its job is followed from here.
      const launched = result.state.presetLaunches.at(-1);
      if (result.message !== null && launched !== undefined) {
        view.presets = trackJob(view.presets, launched);
        paint.send(result.message);
      }
      paint.update(handlers());
    },
    cancelPreset(jobId: string): void {
      view.actions = tappedElsewhere(view.actions);
      paint.send({ type: "cancel_preset_launch", job_id: jobId });
      paint.update(handlers());
    },
  };
}
