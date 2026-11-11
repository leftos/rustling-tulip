import type { PresetLaunchRequest } from "./home-actions";
import {
  CANCEL,
  type CardButton,
  type CardView,
  type GroupView,
  type HomeScreen,
  type LaunchRowView,
  type LaunchView,
  type NewSessionRowView,
  type NewSessionView,
  type PresetRowView,
  type PresetsView,
  type RowView,
} from "./home-screen";
import type { ContainerRef } from "./protocol";

/**
 * Home's DOM: the nodes the painted [`HomeScreen`] draws, and the handler a
 * tap on one asks the view to run. `home-view.ts` only replaces the
 * container's children with them.
 */

/** What a tap on a painted Home node asks the view to do. */
export interface HomeHandlers {
  choose(card: CardView, choice: CardButton): void;
  launch(target: ContainerRef): void;
  toggleNewSession(): void;
  newSession(target: ContainerRef): void;
  togglePresets(target: ContainerRef): void;
  launchPreset(request: PresetLaunchRequest): void;
  cancelPreset(jobId: string): void;
  stop(sessionId: string): void;
  resume(sessionId: string): void;
  move(sessionId: string | null): void;
  open(sessionId: string): void;
}

/**
 * The nodes Home's container holds for `screen`, in order: bar, card tray,
 * the New session button and its open list, groups, empty line.
 */
export function homeNodes(screen: HomeScreen, handlers: HomeHandlers): HTMLElement[] {
  const nodes: HTMLElement[] = [];
  if (screen.bar !== null) {
    const bar = element("p", "home-bar");
    bar.textContent = screen.bar;
    nodes.push(bar);
  }
  if (screen.card !== null) {
    nodes.push(cardPanel(screen.card, handlers));
  }
  if (screen.newSession !== null) {
    nodes.push(...newSessionNodes(screen.newSession, handlers));
  }
  for (const group of screen.groups) {
    nodes.push(groupSection(group, handlers));
  }
  if (screen.empty !== null) {
    const empty = element("p", "home-empty");
    empty.textContent = screen.empty;
    nodes.push(empty);
  }
  return nodes;
}

/** The New session button and, while it is open, its list of containers. */
function newSessionNodes(picker: NewSessionView, handlers: HomeHandlers): HTMLElement[] {
  const nodes: HTMLElement[] = [];
  if (picker.label !== null) {
    nodes.push(
      button("home-new-session", picker.label, picker.enabled, () => {
        handlers.toggleNewSession();
      }),
    );
  }
  if (picker.open) {
    const list = element("div", "home-presets");
    for (const row of picker.rows) {
      list.append(newSessionRow(row, handlers));
    }
    nodes.push(list);
  }
  return nodes;
}

function newSessionRow(row: NewSessionRowView, handlers: HomeHandlers): HTMLElement {
  const node = element("div", "home-row");
  const line = element("div", "home-row-main");
  const name = element("span", "home-row-label");
  name.textContent = row.name;
  line.append(name);
  if (row.label !== null) {
    line.append(
      button("home-new-session-start", row.label, row.enabled, () => {
        handlers.newSession(row.target);
      }),
    );
  }
  node.append(line);
  if (row.error !== null) {
    node.append(errorLine(row.error));
  }
  return node;
}

function cardPanel(card: CardView, handlers: HomeHandlers): HTMLElement {
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
    answers.append(choiceButton(card, choice, handlers));
  }
  if (card.open !== null) {
    answers.append(
      button("home-card-open", card.open, card.openEnabled, () => handlers.open(card.sessionId)),
    );
  }
  panel.append(answers, moves(card, handlers));
  tray.append(panel);
  return tray;
}

function choiceButton(card: CardView, choice: CardButton, handlers: HomeHandlers): HTMLElement {
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
    handlers.choose(card, choice);
  });
  return node;
}

function moves(card: CardView, handlers: HomeHandlers): HTMLElement {
  const row = element("div", "home-card-moves");
  const position = element("span", "home-card-position");
  position.textContent = card.position;
  row.append(
    button("home-card-previous", "Previous", card.previous !== null, () => {
      handlers.move(card.previous);
    }),
    position,
    button("home-card-next", "Next", card.next !== null, () => {
      handlers.move(card.next);
    }),
  );
  return row;
}

function groupSection(group: GroupView, handlers: HomeHandlers): HTMLElement {
  const section = element("section", "home-group");
  const header = element("div", "home-group-header");
  const name = element("h2", "home-group-name");
  name.textContent = group.name;
  header.append(name);
  if (group.presets !== null) {
    const presets = group.presets;
    header.append(
      button("home-group-presets", presets.label, presets.enabled, () => {
        handlers.togglePresets(presets.target);
      }),
    );
  }
  if (group.launch !== null) {
    header.append(launchButton(group.launch, handlers));
  }
  section.append(header);
  if (group.launchError !== null) {
    section.append(errorLine(group.launchError));
  }
  if (group.presets?.open === true) {
    section.append(presetsList(group.presets, handlers));
  }
  for (const row of group.rows) {
    section.append(rowPanel(row, handlers));
  }
  for (const launch of group.launches) {
    section.append(launchRow(launch, handlers));
  }
  return section;
}

function launchButton(launch: LaunchView, handlers: HomeHandlers): HTMLElement {
  return button("home-group-launch", launch.label, launch.enabled, () => {
    handlers.launch(launch.target);
  });
}

function launchRow(row: LaunchRowView, handlers: HomeHandlers): HTMLElement {
  const node = element("div", "home-row");
  const line = element("div", "home-row-main");
  const name = element("span", "home-row-label");
  name.textContent = row.name;
  line.append(name);
  if (row.launch !== null) {
    line.append(launchButton(row.launch, handlers));
  }
  node.append(line);
  if (row.launchError !== null) {
    node.append(errorLine(row.launchError));
  }
  return node;
}

/** An open presets list: the daemon's refusal, or one row per preset. */
function presetsList(presets: PresetsView, handlers: HomeHandlers): HTMLElement {
  const list = element("div", "home-presets");
  if (presets.error !== null) {
    list.append(errorLine(presets.error));
  }
  for (const row of presets.rows) {
    list.append(presetRow(presets, row, handlers));
  }
  return list;
}

function presetRow(presets: PresetsView, row: PresetRowView, handlers: HomeHandlers): HTMLElement {
  const target = presets.target;
  const node = element("div", "home-row");
  const line = element("div", "home-row-main");
  const name = element("span", "home-row-label");
  name.textContent = row.name;
  line.append(name);
  if (row.progress !== null) {
    const progress = element("span", "home-row-status");
    progress.textContent = row.progress;
    line.append(progress);
  }
  const launch = row.launch;
  if (launch !== null) {
    line.append(
      button("home-preset-launch", launch.label, launch.enabled, () => {
        if (launch.source !== null) {
          handlers.launchPreset({ target, presetId: row.presetId, source: launch.source });
        }
      }),
    );
  }
  const jobId = row.cancel;
  if (jobId !== null) {
    line.append(
      button("home-preset-cancel", CANCEL, presets.enabled, () => handlers.cancelPreset(jobId)),
    );
  }
  node.append(line);
  if (row.note !== null) {
    node.append(errorLine(row.note));
  }
  return node;
}

function errorLine(text: string): HTMLElement {
  const error = element("p", "home-row-error");
  error.textContent = text;
  return error;
}

function rowPanel(row: RowView, handlers: HomeHandlers): HTMLElement {
  const node = element("div", "home-row");
  const line = element("div", "home-row-main");
  const label = element("button", "home-row-label");
  label.type = "button";
  label.textContent = row.label;
  label.disabled = !row.enabled;
  label.addEventListener("click", () => {
    handlers.open(row.sessionId);
  });
  const status = element("span", "home-row-status");
  status.textContent = row.status;
  line.append(label, status);
  if (row.stop !== null) {
    line.append(
      button("home-row-stop", row.stop, row.enabled, () => {
        handlers.stop(row.sessionId);
      }),
    );
  }
  if (row.resume) {
    line.append(
      button("home-row-resume", "Resume", row.enabled, () => {
        handlers.resume(row.sessionId);
      }),
    );
  }
  node.append(line);
  if (row.error !== null) {
    node.append(errorLine(row.error));
  }
  return node;
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
