import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";

import type { NewRequestId } from "./home-actions";
import type { ClientMessage } from "./protocol";
import type { Connection } from "./socket";
import { createTerminalPane, type TerminalLike, type TerminalPane } from "./terminal-pane";

/** How often the pane's load timeout and retries advance. */
const TICK_MS = 250;

export interface TerminalViewOptions {
  readonly sessionId: string;
  readonly connection: Connection;
  /** Sends the user's own input; the session screen counts its refusals. */
  readonly sendInput: (message: ClientMessage) => boolean;
  /** The element xterm draws into; the view sizes the terminal to it. */
  readonly container: HTMLElement;
  readonly newRequestId: NewRequestId;
  /** Why the terminal opened: the session screen's attach, or the Terminal toggle. */
  readonly reason: "attach" | "toggle";
  /** Runs after every input xterm reports, so the screen re-reads the Ctrl state. */
  readonly onInput: () => void;
}

/** A mounted terminal: its controller, and the teardown that removes it. */
export interface TerminalView {
  readonly pane: TerminalPane;
  /** Detach the session and take the terminal off the page; a second call does nothing. */
  destroy(): void;
}

/**
 * Draws one session's PTY stream into `container` with xterm.js (its DOM
 * renderer), fitted to the container, and wires it to the connection through
 * the pane controller: the terminal's data goes to the pane, every connection
 * notice reaches it, a resize of the container measures it again, and a timer
 * advances its load.
 */
export function mountTerminal(options: TerminalViewOptions): TerminalView {
  const { sessionId, connection, sendInput, container, newRequestId, reason, onInput } = options;
  const term = new Terminal({
    fontFamily: "ui-monospace, Menlo, Consolas, monospace",
    fontSize: 13,
    scrollback: 5000,
    cursorBlink: false,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(container);
  fit.fit();

  const terminal: TerminalLike = {
    write(data, done) {
      term.write(data, done);
    },
    reset() {
      // RIS through the parser, so the reset lands after any bytes still
      // queued; `term.reset()` acts at once and they would paint over the replay.
      term.write("\x1bc");
    },
  };
  const pane = createTerminalPane({
    sessionId,
    terminal,
    send: (message) => connection.send(message),
    sendInput,
    status: () => connection.status(),
    now: () => Date.now(),
    newRequestId,
  });
  pane.open(term.cols, term.rows, reason);

  const data = term.onData((text) => {
    pane.typed(text);
    onInput();
  });
  const unsubscribe = connection.subscribe((notice) => pane.notice(notice));
  const observer = new ResizeObserver(() => {
    fit.fit();
    // A measure claims only while the user typed recently, so a layout
    // settling after the mount never takes the size by itself.
    pane.measured(term.cols, term.rows, "measure");
  });
  observer.observe(container);
  const timer = window.setInterval(() => pane.tick(), TICK_MS);

  let destroyed = false;
  return {
    pane,
    destroy() {
      if (destroyed) {
        return;
      }
      destroyed = true;
      unsubscribe();
      window.clearInterval(timer);
      observer.disconnect();
      data.dispose();
      pane.close();
      term.dispose();
    },
  };
}
