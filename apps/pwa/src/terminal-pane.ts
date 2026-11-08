import type { NewRequestId } from "./home-actions";
import type { KeyTrayEntry } from "./keys";
import type { ClientMessage } from "./protocol";
import type { ConnectionStateName } from "./connection-machine";
import type { ConnectionNotice } from "./socket";
import {
  INITIAL_KEY_BAR,
  isTerminalReply,
  quickKey,
  replyKey,
  toggleCtrl,
  trayKey,
  typedKey,
  type KeyBarState,
  type KeyTransition,
} from "./terminal-keys";
import {
  INITIAL_TERMINAL,
  attach,
  detach,
  fold,
  ready,
  tick,
  type TerminalState,
  type TerminalTransition,
} from "./terminal-model";
import {
  noteInput,
  resizeFor,
  sessionSize,
  type SizeReason,
  type SizeState,
} from "./terminal-size";

/** What the pane draws into: the subset of xterm's `Terminal` it uses. */
export interface TerminalLike {
  /** Write PTY bytes; `done` runs once xterm has parsed them. */
  write(data: Uint8Array, done: () => void): void;
  /**
   * Clear the screen, scrollback and modes, as a replay starts over. The reset
   * is queued behind earlier writes, so bytes still unparsed land before it.
   */
  reset(): void;
}

export interface TerminalPaneDeps {
  readonly sessionId: string;
  readonly terminal: TerminalLike;
  /** `Connection.send`: false when the connection is not ready. */
  readonly send: (message: ClientMessage) => boolean;
  /** `Connection.status`: the state `open` takes as the last one seen. */
  readonly status: () => ConnectionStateName;
  readonly now: () => number;
  readonly newRequestId: NewRequestId;
}

export interface TerminalPane {
  /** Attach the session and send the pane's first size, claimed (`reason` "attach" or "toggle"). */
  open(cols: number, rows: number, reason: "attach" | "toggle"): void;
  /** One connection notice: daemon messages fold into the load; a `ready` after a drop re-attaches. */
  notice(notice: ConnectionNotice): void;
  /** Text from xterm's `onData`: the terminal's own reply when it has a reply's shape, else typing. */
  typed(text: string): void;
  /** A Keys tray tap. */
  key(entry: KeyTrayEntry): void;
  /** A Quick reply. */
  quick(text: string): void;
  /** The Ctrl button; returns whether Ctrl is now armed. */
  toggleCtrl(): boolean;
  ctrlArmed(): boolean;
  /** The pane was measured at `cols` by `rows`. */
  measured(cols: number, rows: number, reason: "pane-resize" | "measure"): void;
  /** Advance the load's timeout and retries to `now()`. */
  tick(): void;
  /** Detach the session; the pane is inert afterwards. */
  close(): void;
}

/** Where the pane stands: before `open`, open on its session, or closed for good. */
type PaneLife = "new" | "open" | "closed";

/**
 * The phone terminal's controller: it runs the terminal, size and key bar
 * models for one session and carries out what they produce, sending messages
 * through `send` and writing bytes into `terminal`. It knows nothing of the
 * DOM or of xterm itself, so a test drives it with a stub terminal.
 *
 * Every load end (the reply arrived, or the load gave up) and every resync
 * clears the terminal before writing, so a first open, a reconnect and a retry
 * replay the same way. Text the terminal reports in the shape of its own
 * answer to the child (`isTerminalReply`) goes out as an automatic reply,
 * never as typing. Every user input sends the size claim it arms ahead of
 * itself.
 */
export function createTerminalPane(deps: TerminalPaneDeps): TerminalPane {
  const { sessionId, terminal, send } = deps;
  let life: PaneLife = "new";
  let model: TerminalState = INITIAL_TERMINAL;
  let size: SizeState = sessionSize(sessionId);
  let keys: KeyBarState = INITIAL_KEY_BAR;
  let status: ConnectionStateName = deps.status();

  function sendAll(messages: readonly ClientMessage[]): void {
    for (const message of messages) {
      // `false` means offline: nothing is queued, the next `ready` re-attaches.
      send(message);
    }
  }

  /** Stores and sends `cols` by `rows` for `reason`; a size that is not positive sends nothing. */
  function sendSize(cols: number, rows: number, reason: SizeReason): void {
    const next = resizeFor(size, cols, rows, reason, deps.now());
    size = next.state;
    if (next.message !== null) {
      send(next.message);
    }
  }

  function apply(transition: TerminalTransition): void {
    const endsLoad = model.phase === "loading" && transition.state.phase !== "loading";
    model = transition.state;
    sendAll(transition.messages);
    if (endsLoad || transition.reset) {
      terminal.reset();
    }
    for (const bytes of transition.writes) {
      terminal.write(bytes, () => {});
    }
    if (transition.resize) {
      sendSize(size.cols, size.rows, "load");
    }
  }

  /** A user input: its claimed size first when it arms one, then the input. */
  function input(transition: KeyTransition): void {
    keys = transition.state;
    const message = transition.message;
    if (message === null) {
      return;
    }
    const noted = noteInput(size, deps.now());
    size = noted.state;
    if (noted.message !== null) {
      send(noted.message);
    }
    send(message);
  }

  function onStatus(state: ConnectionStateName): void {
    const dropped = status !== "ready";
    status = state;
    if (state !== "ready" || !dropped) {
      return;
    }
    apply(ready(model, deps.newRequestId, deps.now()));
    sendSize(size.cols, size.rows, "reconnect");
  }

  return {
    open(cols, rows, reason) {
      if (life !== "new") {
        return;
      }
      life = "open";
      status = deps.status();
      apply(attach(model, sessionId, deps.newRequestId, deps.now()));
      sendSize(cols, rows, reason);
    },
    notice(notice) {
      if (life !== "open") {
        return;
      }
      switch (notice.kind) {
        case "message":
          apply(fold(model, notice.message));
          return;
        case "status":
          onStatus(notice.state);
          return;
        case "pairing":
          return;
      }
    },
    typed(text) {
      if (life !== "open") {
        return;
      }
      if (isTerminalReply(text)) {
        const reply = replyKey(keys, sessionId, text);
        keys = reply.state;
        if (reply.message !== null) {
          send(reply.message);
        }
        return;
      }
      input(typedKey(keys, sessionId, text));
    },
    key(entry) {
      if (life !== "open") {
        return;
      }
      input(trayKey(keys, sessionId, entry));
    },
    quick(text) {
      if (life !== "open") {
        return;
      }
      input(quickKey(keys, sessionId, text));
    },
    toggleCtrl() {
      if (life === "open") {
        keys = toggleCtrl(keys);
      }
      return keys.ctrl;
    },
    ctrlArmed() {
      return keys.ctrl;
    },
    measured(cols, rows, reason) {
      if (life !== "open" || (cols === size.cols && rows === size.rows)) {
        return;
      }
      sendSize(cols, rows, reason);
    },
    tick() {
      if (life !== "open") {
        return;
      }
      apply(tick(model, deps.now(), deps.newRequestId));
    },
    close() {
      if (life !== "open") {
        life = "closed";
        return;
      }
      life = "closed";
      const left = detach(model);
      model = left.state;
      sendAll(left.messages);
    },
  };
}
