import { INITIAL_CHAT, chatStep, latestPrompt, type ChatState } from "./chat-model";
import { measureScroll, scrollAfterPaint, type ChatScroll } from "./chat-scroll";
import {
  INITIAL_COMPOSER,
  edit,
  foldReply as foldComposer,
  key as keyMessage,
  quick as quickMessage,
  reset as resetComposer,
  submit,
  tick,
  unsent as unsentComposer,
  type ComposerModel,
} from "./composer";
import type { ConnectedLatch } from "./connection-bar";
import type { ConnectionStateName } from "./connection-machine";
import type { HomeModel } from "./home-model";
import type { KeyTrayEntry } from "./keys";
import { renderMarkdown } from "./markdown-view";
import {
  NO_SHEET,
  SHEET_KEYS,
  answerFor,
  foldReply as foldSheet,
  reset as resetSheet,
  resolveAction,
  sheetKey,
  swipe,
  syncSheet,
  unsent as unsentSheet,
  type SheetState,
} from "./prompt-sheet";
import type { ClientMessage, DaemonMessage } from "./protocol";
import type { QuickReplyGroup } from "./quick";
import { planRows, syncChildren, type DrawnRow } from "./row-reuse";
import {
  OFFLINE_NOT_SENT,
  samePart,
  sameRow,
  sameRows,
  sameScreen,
  sessionLook,
  sessionScreen,
  terminalWithSnapshot,
  type AnswerCache,
  type ChatRow,
  type ComposerView,
  type DockKind,
  type DockView,
  type KeyBarView,
  type SessionScreen,
  type SheetButtonView,
  type SheetView,
} from "./session-screen";
import type { Connection } from "./socket";
import { mountTerminal, type TerminalView } from "./terminal-view";
import { sessionBox } from "./viewport";

/** What a session screen is mounted over. */
export interface SessionViewOptions {
  readonly sessionId: string;
  readonly connection: Connection;
  /** Home's model, which names the session and says when it is gone. */
  readonly home: () => HomeModel;
  /** Where the Back button and the browser's own back go. */
  readonly onBack: () => void;
  /** The page's has-connected latch, which Home holds too. */
  readonly connectedOnce: ConnectedLatch;
}

/**
 * One session's screen over one connection: it folds every message into the
 * chat, composer and sheet models, paints the [`SessionScreen`] they make when
 * it differs from the last one drawn, and wires the composer, the Keys and
 * Quick docks and the prompt sheet to the connection. It holds no logic
 * beyond that; the texts all come from the model. Closing it takes its nodes
 * off the page, so nothing it wired can send for the session it left.
 */
export function mountSessionView(container: HTMLElement, options: SessionViewOptions): () => void {
  const { sessionId, connection, onBack, connectedOnce } = options;
  const readHome = options.home;

  let chat: ChatState = INITIAL_CHAT;
  let composer: ComposerModel = INITIAL_COMPOSER;
  let sheet: SheetState = NO_SHEET;
  let dock: DockKind | null = null;
  let status: ConnectionStateName = connection.status();
  /** The header's Terminal toggle: on, the terminal replaces the chat. */
  let terminalOn = false;
  /** The user's own toggle choice; null until they press it. */
  let terminalChoice: boolean | null = null;
  /** The terminal's Ctrl is armed for the next key; re-read after every input. */
  let ctrlArmed = false;
  /** The mounted terminal, while the terminal is shown; null otherwise. */
  let terminalView: TerminalView | null = null;
  /**
   * The chat mirror refused this session once: the terminal stays the main
   * view while the chat reloads after a reconnect or a hide.
   */
  let mirrorRefused = false;
  /** `Offline: not sent` for a dropped Key or Quick tap, until a send lands. */
  let offline: string | null = null;
  /** A send has been refused since the last `ready`, which the bar shows as `Offline`. */
  let sendRefused = false;
  /** `sendUser` refused a send; `terminalInput` clears it before a key bar or dock tap and reads it after. */
  let terminalRefused = false;
  let drawn: SessionScreen | null = null;
  let drawnRows: readonly ChatRow[] | null = null;
  let drawnHidden: string | null = null;
  /** The nodes the chat's rows are drawn with, by row id, reused while a row holds. */
  let drawnChat = new Map<string, DrawnRow<ChatRow, HTMLElement>>();
  /** The hidden-rows line's node, kept until its text changes. */
  let hiddenNode: HTMLElement | null = null;
  let hiddenNodeText: string | null = null;
  /** The chat's position when it last hid, applied once it shows again. */
  let keptScroll: ChatScroll | null = null;
  let drawnKeys: readonly KeyTrayEntry[] | undefined;
  let drawnKeyBar: KeyBarView | null | undefined;
  let drawnComposer: ComposerView | null | undefined;
  let drawnDock: DockView | null | undefined;
  let drawnSheetButtons: readonly SheetButtonView[] | undefined;
  let drawnSheetId: string | null = null;
  let timer: number | undefined;
  const expanded = new Set<string>();
  /** Each answer's parsed Markdown, so the live answer is parsed only when its text grows. */
  const answers: AnswerCache = new Map();

  function newRequestId(): string {
    return crypto.randomUUID();
  }

  const back = element("button", "session-back");
  back.type = "button";
  const title = element("h2", "session-title");
  const toggle = button("session-toggle", "", () => {
    terminalOn = !terminalOn;
    terminalChoice = terminalOn;
    dock = null;
    update();
  });
  const header = element("div", "session-header");
  header.append(back, title, toggle);
  const barEl = element("p", "session-bar");

  const ended = element("p", "session-ended");
  const chatEl = element("div", "session-chat");
  const notice = element("p", "session-notice");
  const terminalEl = element("div", "terminal-pane");
  const keysEl = element("div", "session-keys");
  const keyBarEl = element("div", "session-keybar");
  const dockEl = element("div", "session-dock");

  const draft = document.createElement("textarea");
  draft.className = "session-draft";
  draft.rows = 1;
  const errorEl = element("p", "session-error");
  const sendButton = element("button", "session-send");
  sendButton.type = "submit";
  const keysButton = element("button", "session-keys-button");
  keysButton.type = "button";
  const quickButton = element("button", "session-quick-button");
  quickButton.type = "button";
  const actions = element("div", "session-actions");
  actions.append(sendButton, keysButton, quickButton);
  const composerEl = element("form", "session-composer");
  composerEl.append(draft, errorEl, actions);

  const sheetTitle = element("h2", "session-sheet-title");
  const sheetButtons = element("div", "session-sheet-buttons");
  const sheetBox = document.createElement("textarea");
  sheetBox.className = "session-sheet-text";
  sheetBox.rows = 1;
  const sheetKeys = element("div", "session-sheet-keys");
  for (const key of SHEET_KEYS) {
    const node = button("session-sheet-key", key.label, () => {
      sendUser(sheetKey(sessionId, key.key));
      update();
    });
    sheetKeys.append(node);
  }
  const sheetError = element("p", "session-sheet-error");
  const sheetHide = button("session-sheet-hide", "", () => {
    sheet = swipe(sheet, chat.screen);
    update();
  });
  const sheetEl = element("div", "session-sheet");
  sheetEl.append(sheetTitle, sheetButtons, sheetBox, sheetKeys, sheetError, sheetHide);

  container.append(
    header,
    barEl,
    ended,
    chatEl,
    notice,
    terminalEl,
    keysEl,
    keyBarEl,
    dockEl,
    composerEl,
    sheetEl,
  );

  /** Sends one message and answers whether the connection took it. */
  function sendNow(message: ClientMessage | null): boolean {
    return message === null ? true : connection.send(message);
  }

  /** Sends one message the user asked for, and notes it when the connection refused it. */
  function sendUser(message: ClientMessage): boolean {
    const sent = connection.send(message);
    if (!sent) {
      sendRefused = true;
      terminalRefused = true;
    }
    return sent;
  }

  /**
   * The terminal toggle follows the session's snapshot: a plain shell opens on
   * its terminal, anything else on the chat. A snapshot known at the first
   * paint sets it then, and one arriving later sets it when it comes; once the
   * user presses the toggle, their own choice stands against later snapshots.
   */
  function syncMode(): void {
    const mode = readHome().sessions.find((session) => session.id === sessionId)?.mode;
    if (mode !== undefined) {
      terminalOn = terminalWithSnapshot(terminalChoice, mode);
    }
  }

  function update(): void {
    syncTimer();
    syncMode();
    if (chat.phase === "refused") {
      mirrorRefused = true;
    }
    const look = sessionLook(readHome(), sessionId);
    const screen = sessionScreen({
      title: look.title,
      ended: look.ended,
      isAgent: look.isAgent,
      chat,
      composer,
      sheet,
      dock,
      offline,
      terminal: terminalOn,
      ctrlArmed,
      mirrorRefused,
      status,
      sendRefused,
      connectedOnce: connectedOnce.connected(),
      answers,
    });
    if (drawn !== null && sameScreen(drawn, screen)) {
      return;
    }
    drawn = screen;
    paint(screen);
  }

  /** Paints one screen, rebuilding each part only when what it draws changed. */
  function paint(screen: SessionScreen): void {
    container.hidden = pairingOwnsPage();
    back.textContent = screen.back;
    title.textContent = screen.title;
    title.hidden = screen.title === "";
    toggle.hidden = screen.toggle === null;
    toggle.textContent = screen.toggle ?? "";
    showText(barEl, screen.bar);
    showText(ended, screen.endText);
    showText(notice, screen.notice);
    const rowsSame = sameRows(drawnRows, screen.rows) && drawnHidden === screen.hiddenLine;
    if (!chatEl.hidden && (screen.endText !== null || screen.terminal)) {
      keptScroll = measureScroll(chatEl);
    }
    chatEl.hidden = screen.endText !== null || screen.terminal;
    if (!chatEl.hidden && keptScroll !== null && rowsSame) {
      const target = scrollAfterPaint(keptScroll, measureScroll(chatEl), chatEl.scrollHeight);
      if (target !== null) {
        chatEl.scrollTop = target;
      }
      keptScroll = null;
    }
    terminalEl.hidden = !screen.terminal;
    composerEl.hidden = screen.composer === null;
    keysEl.hidden = screen.keys.length === 0;
    keyBarEl.hidden = screen.keyBar === null;
    dockEl.hidden = screen.dock === null;
    if (drawnKeys === undefined || !samePart(drawnKeys, screen.keys)) {
      drawnKeys = screen.keys;
      paintKeys(screen.keys);
    }
    if (drawnKeyBar === undefined || !samePart(drawnKeyBar, screen.keyBar)) {
      drawnKeyBar = screen.keyBar;
      paintKeyBar(screen.keyBar);
    }
    if (drawnComposer === undefined || !samePart(drawnComposer, screen.composer)) {
      drawnComposer = screen.composer;
      paintComposer(screen.composer);
    }
    if (drawnDock === undefined || !samePart(drawnDock, screen.dock)) {
      drawnDock = screen.dock;
      paintDock(screen.dock);
    }
    paintSheet(screen.sheet);
    if (!rowsSame) {
      drawnRows = screen.rows;
      drawnHidden = screen.hiddenLine;
      paintChat(screen);
    }
    syncTerminal(screen);
  }

  /**
   * Draws the chat's rows, rebuilding only the ones whose row changed so an
   * earlier answer keeps its own scroll and selection, and keeps the newest row
   * in view only when it was.
   */
  function paintChat(screen: SessionScreen): void {
    const before = measureScroll(chatEl);
    if (hiddenNodeText !== screen.hiddenLine) {
      hiddenNode = null;
      hiddenNodeText = screen.hiddenLine;
      if (screen.hiddenLine !== null) {
        const line = element("p", "session-hidden");
        line.textContent = screen.hiddenLine;
        hiddenNode = line;
      }
    }
    const plan = planRows(drawnChat, screen.rows, sameRow, rowNode);
    drawnChat = new Map(plan.map((entry) => [entry.row.id, entry]));
    const nodes = plan.map((entry) => entry.node);
    syncChildren(chatEl, hiddenNode === null ? nodes : [hiddenNode, ...nodes]);
    if (!chatEl.hidden) {
      const target = scrollAfterPaint(keptScroll, before, chatEl.scrollHeight);
      if (target !== null) {
        chatEl.scrollTop = target;
      }
      keptScroll = null;
    }
  }

  function rowNode(row: ChatRow): HTMLElement {
    switch (row.kind) {
      case "user":
        return bubble(row, "session-user");
      case "answer":
        return bubble(row, "session-answer");
      case "tool":
        return toolNode(row);
      default:
        return mutedNode(row);
    }
  }

  function bubble(row: ChatRow, className: string): HTMLElement {
    let node: HTMLElement;
    if (row.kind === "answer" && row.markdown !== undefined) {
      // The agent's answer as Markdown; the view builds text nodes only (markdown-view.ts).
      node = element("div", `session-row ${className} session-md`);
      node.append(renderMarkdown(document, row.markdown));
    } else {
      node = element("p", `session-row ${className}`);
      node.textContent = row.text;
    }
    if (row.pending) {
      node.classList.add("session-pending");
    }
    if (row.live) {
      node.classList.add("session-live");
    }
    return node;
  }

  function mutedNode(row: ChatRow): HTMLElement {
    const node = element("p", "session-muted");
    node.textContent = row.text;
    return node;
  }

  /** A tool call's one collapsed line, which a tap opens to its input and result. */
  function toolNode(row: ChatRow): HTMLElement {
    const wrap = element("div", "session-tool");
    const head = button("session-tool-head", row.text, () => {
      if (expanded.has(row.id)) {
        expanded.delete(row.id);
      } else {
        expanded.add(row.id);
      }
      detail.hidden = !expanded.has(row.id);
    });
    const detail = element("p", "session-tool-detail");
    detail.textContent = row.detail ?? "";
    detail.hidden = row.detail === null || !expanded.has(row.id);
    wrap.append(head, detail);
    return wrap;
  }

  function paintKeys(keys: readonly KeyTrayEntry[]): void {
    keysEl.replaceChildren(
      ...keys.map((entry) =>
        button("session-key", entry.label, () => sendInput(keyMessage(sessionId, entry.bytes))),
      ),
    );
  }

  /** Rebuilds the key bar under the terminal: the Keys tray, Ctrl and the Quick dock's button. */
  function paintKeyBar(view: KeyBarView | null): void {
    if (view === null) {
      keyBarEl.replaceChildren();
      return;
    }
    const keys = view.keys.map((entry) =>
      button("session-key", entry.label, () => terminalInput(() => terminalView?.pane.key(entry))),
    );
    const ctrl = button("session-key session-ctrl", view.ctrl, () => {
      ctrlArmed = terminalView?.pane.toggleCtrl() ?? false;
      update();
    });
    if (view.ctrlArmed) {
      ctrl.classList.add("session-selected");
    }
    const quick = button("session-key session-keybar-quick", view.quick, () => {
      dock = dock === "quick" ? null : "quick";
      update();
    });
    for (const node of [...keys, ctrl, quick]) {
      keepTerminalFocus(node);
    }
    keyBarEl.replaceChildren(...keys, ctrl, quick);
  }

  /**
   * Mounts the terminal once the pane is shown, and unmounts it once it is
   * not: every shown stretch mounts a fresh pane, so a toggle off and on again
   * attaches again.
   */
  function syncTerminal(screen: SessionScreen): void {
    if (!screen.terminal) {
      if (terminalView !== null) {
        terminalView.destroy();
        terminalView = null;
        terminalEl.replaceChildren();
        ctrlArmed = false;
      }
      return;
    }
    if (terminalView !== null) {
      return;
    }
    terminalView = mountTerminal({
      sessionId,
      connection,
      sendInput: sendUser,
      container: terminalEl,
      newRequestId,
      reason: terminalOn ? "toggle" : "attach",
      onInput: () => {
        ctrlArmed = terminalView?.pane.ctrlArmed() ?? false;
        update();
      },
    });
  }

  function paintComposer(view: ComposerView | null): void {
    if (view === null) {
      return;
    }
    if (draft.value !== view.draft) {
      draft.value = view.draft;
    }
    draft.placeholder = view.placeholder;
    sendButton.textContent = view.send;
    sendButton.disabled = !view.sendEnabled;
    keysButton.textContent = view.keys;
    quickButton.textContent = view.quick;
    showText(errorEl, view.error);
  }

  function paintDock(view: DockView | null): void {
    if (view === null) {
      dockEl.replaceChildren();
      return;
    }
    const nodes =
      view.kind === "keys"
        ? view.keys.map((entry) =>
            button("session-key", entry.label, () => sendInput(keyMessage(sessionId, entry.bytes))),
          )
        : view.groups.map(quickGroupNode);
    dockEl.replaceChildren(...nodes);
  }

  function quickGroupNode(group: QuickReplyGroup): HTMLElement {
    const section = element("div", "session-quick-group");
    const name = element("p", "session-quick-title");
    name.textContent = group.title;
    const row = element("div", "session-quick-items");
    for (const text of group.items) {
      const reply = button("session-quick-reply", text, () => {
        const view = terminalView;
        if (view === null) {
          sendInput(quickMessage(sessionId, text));
        } else {
          terminalInput(() => view.pane.quick(text));
        }
      });
      // In terminal mode the tap keeps xterm's focus and the soft keyboard; in
      // chat mode it behaves as any button.
      reply.addEventListener("pointerdown", (event) => {
        if (terminalView !== null) {
          event.preventDefault();
        }
      });
      row.append(reply);
    }
    section.append(name, row);
    return section;
  }

  function paintSheet(view: SheetView | null): void {
    sheetEl.hidden = view === null;
    if (view === null) {
      drawnSheetId = null;
      return;
    }
    sheetTitle.textContent = view.title;
    sheetBox.hidden = !view.typing;
    showText(sheetError, view.error);
    sheetHide.textContent = view.hide;
    if (drawnSheetId !== view.id) {
      drawnSheetId = view.id;
      sheetBox.value = "";
    }
    if (drawnSheetButtons === undefined || !samePart(drawnSheetButtons, view.buttons)) {
      drawnSheetButtons = view.buttons;
      sheetButtons.replaceChildren(...view.buttons.map(sheetButtonNode));
    }
  }

  /** One answer button: its option's number as a key-cap before its label. */
  function sheetButtonNode(entry: SheetButtonView): HTMLElement {
    const node = element("button", "session-sheet-button");
    node.type = "button";
    if (entry.number !== null) {
      const cap = element("span", "session-sheet-number");
      cap.textContent = entry.number;
      node.append(cap);
    }
    const label = element("span", "session-sheet-label");
    label.textContent = entry.label;
    node.append(label);
    if (entry.selected) {
      node.classList.add("session-selected");
    }
    node.addEventListener("click", () => {
      answerPrompt(entry);
    });
    return node;
  }

  function answerPrompt(choice: SheetButtonView): void {
    const prompt = latestPrompt(chat);
    if (prompt === null) {
      return;
    }
    const answer = resolveAction(choice.action, sheetBox.value);
    if (answer === null) {
      return;
    }
    const result = answerFor(sheet, sessionId, prompt, answer, newRequestId);
    sheet = result.state;
    if (result.message !== null) {
      if (sendUser(result.message)) {
        offline = null;
        sheetBox.value = "";
      } else {
        // Nothing left, so a second answer is not blocked behind a record of
        // one the connection never sent.
        sheet = unsentSheet(sheet, prompt.id, OFFLINE_NOT_SENT);
      }
    }
    update();
  }

  function sendDraft(): void {
    const result = submit(composer, sessionId, newRequestId, Date.now());
    composer = result.state;
    const sending = composer.sending;
    if (result.message !== null && sending !== null) {
      if (sendUser(result.message)) {
        offline = null;
      } else {
        composer = unsentComposer(composer, sending.requestId, OFFLINE_NOT_SENT);
      }
    }
    update();
  }

  /** A Key or Quick tap: a dropped one shows the offline line until a send lands. */
  function sendInput(message: ClientMessage): void {
    offline = sendUser(message) ? null : OFFLINE_NOT_SENT;
    update();
  }

  /**
   * A key bar or terminal-dock input, typed into the pane; the Ctrl state it
   * may have consumed is re-read afterwards. Nothing happens while no
   * terminal is mounted.
   */
  function terminalInput(act: () => void): void {
    if (terminalView === null) {
      return;
    }
    terminalRefused = false;
    act();
    offline = terminalRefused ? OFFLINE_NOT_SENT : null;
    ctrlArmed = terminalView.pane.ctrlArmed();
    update();
  }

  /** Runs the send backstop's clock only while a send is out. */
  function syncTimer(): void {
    if (composer.sending !== null && timer === undefined) {
      timer = window.setInterval(() => {
        composer = tick(composer, Date.now());
        update();
      }, 1000);
    } else if (composer.sending === null && timer !== undefined) {
      window.clearInterval(timer);
      timer = undefined;
    }
  }

  /** Whether the pairing screen owns the page right now. */
  function pairingOwnsPage(): boolean {
    return status === "pairing" || status === "notApproved";
  }

  /** Applies a chat transition's state and sends what it asks for. */
  function apply(transition: {
    readonly state: ChatState;
    readonly effects: readonly ClientMessage[];
  }): void {
    chat = transition.state;
    for (const effect of transition.effects) {
      sendNow(effect);
    }
  }

  function onMessage(message: DaemonMessage): void {
    apply(chatStep(chat, { type: "message", message }, newRequestId));
    composer = foldComposer(composer, sessionId, message);
    sheet = syncSheet(foldSheet(sheet, message), chat.screen);
    update();
  }

  back.addEventListener("click", onBack);
  composerEl.addEventListener("submit", (event) => {
    event.preventDefault();
    sendDraft();
  });
  keysButton.addEventListener("click", () => {
    dock = dock === "keys" ? null : "keys";
    update();
  });
  quickButton.addEventListener("click", () => {
    dock = dock === "quick" ? null : "quick";
    update();
  });
  draft.addEventListener("input", () => {
    composer = edit(composer, draft.value);
    update();
  });

  const unsubscribe = connection.subscribe((notice) => {
    if (notice.kind === "pairing") {
      return;
    }
    if (notice.kind === "status") {
      status = notice.state;
      if (status === "ready") {
        if (connection.paired()) {
          connectedOnce.mark();
        }
        composer = resetComposer(composer);
        sheet = resetSheet(sheet);
        offline = null;
        sendRefused = false;
        // A chat dropped while the page was hidden is entered again; one still
        // held reloads, since the daemon resends its state only to a fresh load.
        apply(
          chatStep(
            chat,
            chat.sessionId === null ? { type: "enter", sessionId } : { type: "ready" },
            newRequestId,
          ),
        );
      }
      if (status === "hidden") {
        apply(chatStep(chat, { type: "hidden" }, newRequestId));
      }
      container.hidden = pairingOwnsPage();
      update();
      return;
    }
    onMessage(notice.message);
  });
  apply(chatStep(chat, { type: "enter", sessionId }, newRequestId));
  update();
  const stopViewport = trackViewport(container);

  return () => {
    // Off the page first: nothing this view wired can fire again.
    stopViewport();
    container.replaceChildren();
    container.hidden = true;
    terminalView?.destroy();
    terminalView = null;
    unsubscribe();
    if (timer !== undefined) {
      window.clearInterval(timer);
      timer = undefined;
    }
    apply(chatStep(chat, { type: "leave" }, newRequestId));
  };
}

/**
 * Keeps `container` over the visual viewport while the screen is mounted, as
 * `--vv-top` and `--vv-height`, which `.session` is sized from, and marks it
 * `data-keyboard="open"` while the keyboard is up. A browser
 * without `visualViewport` keeps the screen at its full height. The returned
 * function removes the listeners and the variables.
 */
function trackViewport(container: HTMLElement): () => void {
  const viewport = window.visualViewport;
  if (viewport === null) {
    return () => {};
  }
  const place = (): void => {
    const box = sessionBox(viewport, window.innerHeight);
    container.style.setProperty("--vv-top", `${box.top}px`);
    container.style.setProperty("--vv-height", `${box.height}px`);
    if (box.keyboardOpen) {
      container.dataset["keyboard"] = "open";
    } else {
      delete container.dataset["keyboard"];
    }
  };
  viewport.addEventListener("resize", place);
  viewport.addEventListener("scroll", place);
  place();
  return () => {
    viewport.removeEventListener("resize", place);
    viewport.removeEventListener("scroll", place);
    container.style.removeProperty("--vv-top");
    container.style.removeProperty("--vv-height");
    delete container.dataset["keyboard"];
  };
}

function showText(node: HTMLElement, text: string | null): void {
  node.hidden = text === null;
  node.textContent = text ?? "";
}

/**
 * Keeps a tap on `node` from taking the focus off xterm's hidden textarea,
 * which would close the phone's soft keyboard.
 */
function keepTerminalFocus(node: HTMLElement): void {
  node.addEventListener("pointerdown", (event) => {
    event.preventDefault();
  });
}

function button(className: string, label: string, onClick: () => void): HTMLButtonElement {
  const node = element("button", className);
  node.type = "button";
  node.textContent = label;
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
