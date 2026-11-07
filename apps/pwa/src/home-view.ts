import {
  answer,
  foldReply,
  reset as resetActions,
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
  type RowView,
} from "./home-screen";
import type { ConnectedLatch } from "./connection-bar";
import type { ConnectionStateName } from "./connection-machine";
import { introLineShown } from "./installed";
import type { ClientMessage } from "./protocol";
import type { Connection } from "./socket";

/** The page's intro header, its install line, and whether the page runs from the Home Screen. */
export interface Intro {
  readonly header: HTMLElement;
  readonly line: HTMLElement | null;
  readonly installed: boolean;
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
  let model: HomeModel = resetModel();
  let actions: HomeActions = resetActions();
  let status: ConnectionStateName = "idle";
  let cardId: string | null = null;
  let lastIndex = 0;
  let offline = false;
  let drawn: HomeScreen | null = null;

  function newRequestId(): string {
    return crypto.randomUUID();
  }

  function send(message: ClientMessage): void {
    if (!connection.send(message)) {
      offline = true;
    }
  }

  function update(): void {
    onModel(model);
    const state: HomeState = {
      status,
      paired: connection.paired(),
      shown: connectedOnce.connected(),
      card: cardId,
      index: lastIndex,
      offline,
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
    const name = element("h2", "home-group-name");
    name.textContent = group.name;
    section.append(name);
    for (const row of group.rows) {
      section.append(rowPanel(row));
    }
    return section;
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
      const error = element("p", "home-row-error");
      error.textContent = row.error;
      node.append(error);
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
    const result = resume(actions, newRequestId, session);
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
      // so nothing from the old connection is kept.
      if (status === "ready") {
        model = resetModel();
        actions = resetActions();
        cardId = null;
        lastIndex = 0;
        offline = false;
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

  return unsubscribe;
}
