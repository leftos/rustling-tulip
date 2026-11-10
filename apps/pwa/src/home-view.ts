import {
  answer,
  expireLaunches,
  foldReply,
  keepLaunches,
  launchLast,
  reset as resetActions,
  resendLaunches,
  resume,
  stop,
  type HomeActions,
} from "./home-actions";
import { homeStep, reset as resetModel, type HomeModel } from "./home-model";
import {
  homeScreen,
  sameScreen,
  tappedElsewhere,
  updatedSession,
  type CardButton,
  type CardView,
  type GroupView,
  type HomeScreen,
  type HomeState,
  type LaunchRowView,
  type LaunchView,
  type RowView,
} from "./home-screen";
import type { ConnectedLatch } from "./connection-bar";
import type { ConnectionStateName } from "./connection-machine";
import { introLineShown } from "./installed";
import {
  FEATURE_LAUNCH_LAST,
  FEATURE_RESUME_SESSION,
  type ClientMessage,
  type ContainerRef,
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
  let model: HomeModel = resetModel();
  let actions: HomeActions = resetActions();
  let status: ConnectionStateName = "idle";
  let cardId: string | null = null;
  let lastIndex = 0;
  let offline = false;
  let lastSupportsLaunchLast = false;
  let drawn: HomeScreen | null = null;
  let expiry: ReturnType<typeof setInterval> | null = null;

  function newRequestId(): string {
    return crypto.randomUUID();
  }

  function send(message: ClientMessage): void {
    if (!connection.send(message)) {
      offline = true;
    }
  }

  function syncExpiry(): void {
    const waiting = actions.launches.length > 0;
    if (waiting && expiry === null) {
      expiry = setInterval(expire, LAUNCH_EXPIRY_TICK_MS);
    } else if (!waiting && expiry !== null) {
      clearInterval(expiry);
      expiry = null;
    }
  }

  function expire(): void {
    const next = expireLaunches(actions, Date.now());
    if (next === actions) {
      return;
    }
    actions = next;
    update();
  }

  function update(): void {
    onModel(model);
    syncExpiry();
    const state: HomeState = {
      status,
      paired: connection.paired(),
      shown: connectedOnce.connected(),
      card: cardId,
      index: lastIndex,
      offline,
      supportsLaunchLast: lastSupportsLaunchLast,
    };
    const screen = homeScreen(model, actions, state);
    if (screen.visible) {
      connectedOnce.mark();
    }
    cardId = screen.card?.sessionId ?? null;
    lastIndex = screen.index;
    // A repaint rebuilds every button, so a tap lands on a node that is
    // already gone when the screen draws the same thing anyway.
    if (drawn !== null && sameScreen(drawn, screen)) {
      return;
    }
    drawn = screen;
    paint(screen);
  }

  function paint(screen: HomeScreen): void {
    container.hidden = !screen.visible;
    if (intro !== null) {
      intro.header.hidden = screen.visible;
    }
    const nodes: HTMLElement[] = [];
    if (screen.bar !== null) {
      const bar = element("p", "home-bar");
      bar.textContent = screen.bar;
      nodes.push(bar);
    }
    if (screen.card !== null) {
      nodes.push(cardPanel(screen.card));
    }
    for (const group of screen.groups) {
      nodes.push(groupSection(group));
    }
    if (screen.empty !== null) {
      const empty = element("p", "home-empty");
      empty.textContent = screen.empty;
      nodes.push(empty);
    }
    container.replaceChildren(...nodes);
  }

  function cardPanel(card: CardView): HTMLElement {
    const tray = element("div", "home-tray");
    const panel = element("div", "home-card");
    const label = element("p", "home-card-label");
    label.textContent = card.label;
    const detail = element("p", "home-card-detail");
    detail.textContent = card.detail;
    panel.append(label, detail);
    if (card.sending !== null) {
      const sending = element("p", "home-card-sending");
      sending.textContent = card.sending;
      panel.append(sending);
    }
    if (card.error !== null) {
      const error = element("p", "home-card-error");
      error.textContent = card.error;
      panel.append(error);
    }
    const answers = element("div", "home-card-answers");
    for (const choice of card.buttons) {
      answers.append(choiceButton(card, choice));
    }
    if (card.open !== null) {
      answers.append(
        button("home-card-open", card.open, card.openEnabled, () => openSession(card.sessionId)),
      );
    }
    panel.append(answers, moves(card));
    tray.append(panel);
    return tray;
  }

  function choiceButton(card: CardView, choice: CardButton): HTMLElement {
    const node = document.createElement("button");
    node.type = "button";
    node.className = "home-choice";
    node.disabled = !choice.enabled;
    if (choice.hint !== null) {
      const hint = element("span", "home-hint");
      hint.textContent = choice.hint;
      node.append(hint);
    }
    const text = element("span", "home-choice-label");
    text.textContent = choice.label;
    node.append(text);
    node.addEventListener("click", () => {
      choose(card, choice);
    });
    return node;
  }

  function moves(card: CardView): HTMLElement {
    const row = element("div", "home-card-moves");
    const position = element("span", "home-card-position");
    position.textContent = card.position;
    row.append(
      button("home-card-previous", "Previous", card.previous !== null, () => {
        moveTo(card.previous);
      }),
      position,
      button("home-card-next", "Next", card.next !== null, () => {
        moveTo(card.next);
      }),
    );
    return row;
  }

  function groupSection(group: GroupView): HTMLElement {
    const section = element("section", "home-group");
    const header = element("div", "home-group-header");
    const name = element("h2", "home-group-name");
    name.textContent = group.name;
    header.append(name);
    if (group.launch !== null) {
      header.append(launchButton(group.launch));
    }
    section.append(header);
    if (group.launchError !== null) {
      section.append(errorLine(group.launchError));
    }
    for (const row of group.rows) {
      section.append(rowPanel(row));
    }
    for (const launch of group.launches) {
      section.append(launchRow(launch));
    }
    return section;
  }

  function launchButton(launch: LaunchView): HTMLElement {
    return button("home-group-launch", launch.label, launch.enabled, () => {
      launchContainer(launch.target);
    });
  }

  function launchRow(row: LaunchRowView): HTMLElement {
    const node = element("div", "home-row");
    const line = element("div", "home-row-main");
    const name = element("span", "home-row-label");
    name.textContent = row.name;
    line.append(name);
    if (row.launch !== null) {
      line.append(launchButton(row.launch));
    }
    node.append(line);
    if (row.launchError !== null) {
      node.append(errorLine(row.launchError));
    }
    return node;
  }

  function errorLine(text: string): HTMLElement {
    const error = element("p", "home-row-error");
    error.textContent = text;
    return error;
  }

  function rowPanel(row: RowView): HTMLElement {
    const node = element("div", "home-row");
    const line = element("div", "home-row-main");
    const label = element("button", "home-row-label");
    label.type = "button";
    label.textContent = row.label;
    label.disabled = !row.enabled;
    label.addEventListener("click", () => {
      openSession(row.sessionId);
    });
    const status = element("span", "home-row-status");
    status.textContent = row.status;
    line.append(label, status);
    if (row.stop !== null) {
      line.append(
        button("home-row-stop", row.stop, row.enabled, () => {
          stopSession(row.sessionId);
        }),
      );
    }
    if (row.resume) {
      line.append(
        button("home-row-resume", "Resume", row.enabled, () => {
          resumeSession(row.sessionId);
        }),
      );
    }
    node.append(line);
    if (row.error !== null) {
      node.append(errorLine(row.error));
    }
    return node;
  }

  function choose(card: CardView, choice: CardButton): void {
    if (card.since === null) {
      return;
    }
    actions = tappedElsewhere(actions);
    const result = answer(actions, newRequestId, card.sessionId, card.since, choice.choice);
    actions = result.state;
    if (result.message !== null) {
      send(result.message);
    }
    update();
  }

  function launchContainer(target: ContainerRef): void {
    actions = tappedElsewhere(actions);
    const result = launchLast(actions, target, newRequestId, Date.now());
    actions = result.state;
    if (result.message !== null) {
      send(result.message);
    }
    update();
  }

  function stopSession(sessionId: string): void {
    const result = stop(actions, sessionId);
    actions = result.state;
    if (result.message !== null) {
      send(result.message);
    }
    update();
  }

  function resumeSession(sessionId: string): void {
    const session = model.sessions.find((known) => known.id === sessionId);
    if (session === undefined) {
      return;
    }
    actions = tappedElsewhere(actions);
    const supportsResumeSession = connection.supports(FEATURE_RESUME_SESSION);
    const result = resume(actions, newRequestId, session, supportsResumeSession);
    actions = result.state;
    if (result.message !== null) {
      send(result.message);
    }
    update();
  }

  function moveTo(sessionId: string | null): void {
    if (sessionId === null) {
      return;
    }
    actions = tappedElsewhere(actions);
    cardId = sessionId;
    update();
  }

  function openSession(sessionId: string): void {
    actions = tappedElsewhere(actions);
    onOpen(sessionId);
    update();
  }

  function button(
    className: string,
    label: string,
    enabled: boolean,
    onClick: () => void,
  ): HTMLButtonElement {
    const node = element("button", className);
    node.type = "button";
    node.textContent = label;
    node.disabled = !enabled;
    node.addEventListener("click", onClick);
    return node;
  }

  function element<K extends keyof HTMLElementTagNameMap>(
    tag: K,
    className: string,
  ): HTMLElementTagNameMap[K] {
    const node = document.createElement(tag);
    node.className = className;
    return node;
  }

  // The install line never changes in a page visit; Home hides the whole header anyway.
  if (intro !== null && intro.line !== null) {
    intro.line.hidden = !introLineShown(intro.installed);
  }

  const unsubscribe = connection.subscribe((notice) => {
    if (notice.kind === "status") {
      status = notice.state;
      // Every `ready` is a fresh start: the daemon resends its whole state,
      // so nothing from the old connection is kept, save a launch still
      // waiting for its answer, which is resent with its id.
      if (status === "ready") {
        model = resetModel();
        actions = keepLaunches(actions);
        lastSupportsLaunchLast = connection.supports(FEATURE_LAUNCH_LAST);
        cardId = null;
        lastIndex = 0;
        offline = false;
        const resent = resendLaunches(actions, lastSupportsLaunchLast, Date.now());
        actions = resent.state;
        for (const message of resent.messages) {
          send(message);
        }
      }
      update();
      return;
    }
    if (notice.kind === "message") {
      model = homeStep(model, notice.message);
      const folded = foldReply(actions, notice.message);
      actions = folded.state;
      if (notice.message.type === "session_updated") {
        actions = updatedSession(actions, notice.message.session.id);
      }
      for (const effect of folded.effects) {
        send(effect);
      }
      update();
    }
  });
  update();

  return () => {
    unsubscribe();
    if (expiry !== null) {
      clearInterval(expiry);
      expiry = null;
    }
  };
}
