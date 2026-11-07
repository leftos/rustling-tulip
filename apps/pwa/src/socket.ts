import {
  step,
  type ConnectionEffect,
  type ConnectionEvent,
  type ConnectionState,
  type ConnectionStateName,
} from "./connection-machine";
import type { DaemonMessage } from "./protocol";
import type { TokenStore } from "./token-store";

/** The subset of `WebSocket` the connection uses, so a test can stand one in. */
export interface WebSocketLike {
  send(data: string): void;
  close(): void;
  onopen: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: ((event: unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
}

export type TimerHandle = ReturnType<typeof setTimeout>;

/** The page's background state: iOS suspends a backgrounded web app anyway. */
export interface Visibility {
  isHidden(): boolean;
  onChange(callback: () => void): () => void;
}

export interface ConnectionDeps {
  /** `wss://{the page's own host}/ws`: the daemon's Origin check admits no other. */
  readonly url: string;
  openSocket(url: string): WebSocketLike;
  setTimeout(callback: () => void, delayMs: number): TimerHandle;
  clearTimeout(handle: TimerHandle): void;
  readonly visibility: Visibility;
  readonly tokenStore: TokenStore;
}

/**
 * What {@link Connection.subscribe} delivers: each daemon message the page is
 * meant to see, and each state the connection entered, in order. A listener
 * treats every `ready` as a fresh start (the daemon re-sends its initial
 * state after a reconnect) and stops acting on anything but `ready`.
 */
export type ConnectionNotice =
  | { readonly kind: "message"; readonly message: DaemonMessage }
  | { readonly kind: "status"; readonly state: ConnectionStateName };

export interface Connection {
  start(): void;
  stop(): void;
  /** The stored token changed (pairing finished, or the device was forgotten). */
  tokenChanged(): void;
  subscribe(listener: (notice: ConnectionNotice) => void): () => void;
}

/**
 * Runs the connection state machine over one socket: it opens the socket the
 * machine asks for, feeds frames back as events, and runs its effects. One
 * socket and at most one timer live at a time; a replaced socket's late
 * events are dropped.
 */
export function createConnection(deps: ConnectionDeps): Connection {
  let state: ConnectionState = { name: "idle" };
  let socket: WebSocketLike | undefined;
  let socketGeneration = 0;
  let timer: TimerHandle | undefined;
  let unsubscribeVisibility: (() => void) | undefined;
  let started = false;
  let stopped = false;
  const listeners = new Set<(notice: ConnectionNotice) => void>();

  function notify(notice: ConnectionNotice): void {
    for (const listener of listeners) {
      listener(notice);
    }
  }

  function closeSocket(): void {
    socketGeneration += 1;
    const current = socket;
    socket = undefined;
    current?.close();
  }

  function clearTimer(): void {
    if (timer !== undefined) {
      deps.clearTimeout(timer);
      timer = undefined;
    }
  }

  function onFrame(data: unknown, generation: number): void {
    if (generation !== socketGeneration) {
      return;
    }
    if (typeof data !== "string") {
      console.warn("pwa: dropped a non-text frame");
      return;
    }
    let message: unknown;
    try {
      message = JSON.parse(data);
    } catch {
      console.warn("pwa: dropped an unparseable frame");
      return;
    }
    dispatch({ type: "message", message }, generation);
  }

  function openSocket(token: string | undefined, generation: number): void {
    if (stopped || generation !== socketGeneration) {
      return;
    }
    const opened = deps.openSocket(deps.url);
    socket = opened;
    opened.onopen = () => {
      dispatch({ type: "opened", token }, generation);
    };
    opened.onmessage = (event) => {
      onFrame(event.data, generation);
    };
    opened.onclose = () => {
      dispatch({ type: "closed" }, generation);
    };
    opened.onerror = (event) => {
      console.warn("pwa: socket error", event);
      dispatch({ type: "closed" }, generation);
    };
  }

  function connect(): void {
    closeSocket();
    const generation = socketGeneration;
    void deps.tokenStore.get().then(
      (token) => {
        openSocket(token, generation);
      },
      (error: unknown) => {
        // A store that cannot answer is a connection that cannot open: back
        // off and ask it again on the next attempt.
        console.warn("pwa: reading the device token failed", error);
        dispatch({ type: "closed" }, generation);
      },
    );
  }

  function run(effect: ConnectionEffect): void {
    switch (effect.type) {
      case "open":
        connect();
        break;
      case "send":
        socket?.send(JSON.stringify(effect.message));
        break;
      case "close":
        closeSocket();
        break;
      case "setTimer":
        clearTimer();
        timer = deps.setTimeout(() => {
          timer = undefined;
          dispatch({ type: "timer" });
        }, effect.delayMs);
        break;
      case "clearTimer":
        clearTimer();
        break;
      case "deliver":
        notify({ kind: "message", message: effect.message });
        break;
      case "status":
        notify({ kind: "status", state: effect.state });
        break;
    }
  }

  function dispatch(event: ConnectionEvent, generation?: number): void {
    if (stopped) {
      return;
    }
    if (generation !== undefined && generation !== socketGeneration) {
      return;
    }
    const result = step(state, event);
    state = result.state;
    for (const effect of result.effects) {
      run(effect);
    }
  }

  function start(): void {
    if (started || stopped) {
      return;
    }
    started = true;
    unsubscribeVisibility = deps.visibility.onChange(() => {
      dispatch({ type: deps.visibility.isHidden() ? "hiddenEvent" : "visible" });
    });
    // A page first loaded while hidden opens nothing: `hidden` holds no
    // socket and no timer, and the first `visible` dials.
    dispatch({ type: deps.visibility.isHidden() ? "hiddenEvent" : "start" });
  }

  function stop(): void {
    if (stopped) {
      return;
    }
    stopped = true;
    clearTimer();
    closeSocket();
    unsubscribeVisibility?.();
    unsubscribeVisibility = undefined;
    listeners.clear();
  }

  function tokenChanged(): void {
    dispatch({ type: "tokenChanged" });
  }

  function subscribe(listener: (notice: ConnectionNotice) => void): () => void {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }

  return { start, stop, tokenChanged, subscribe };
}
