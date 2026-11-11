import {
  step,
  type ConnectionEffect,
  type ConnectionEvent,
  type ConnectionState,
  type ConnectionStateName,
} from "./connection-machine";
import {
  INITIAL_PAIRING,
  canAsk,
  isPairingCode,
  pairingStep,
  type PairingEvent,
  type PairingState,
} from "./pairing";
import type { ClientMessage, DaemonMessage } from "./protocol";
import type { TokenStore } from "./token-store";

/** The subset of `WebSocket` the connection uses, so a test can stand one in. */
export interface WebSocketLike {
  /**
   * `1` while the socket is open (`WebSocket.OPEN`). A socket that reports
   * anything else is written nothing, since a write on a closing or closed
   * socket is dropped without an error.
   */
  readonly readyState: number;
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
  /**
   * Reload the page, as a host's `reload_app` asks. Injected so a test can
   * see it, and so nothing but `main.ts` touches `location`.
   */
  reloadApp(): void;
}

/**
 * What {@link Connection.subscribe} delivers: each daemon message the page is
 * meant to see, each state the connection entered, and each new pairing
 * model, in order. A listener treats every `ready` as a fresh start (the
 * daemon re-sends its initial state after a reconnect) and stops acting on
 * anything but `ready`.
 */
export type ConnectionNotice =
  | { readonly kind: "message"; readonly message: DaemonMessage }
  | { readonly kind: "status"; readonly state: ConnectionStateName }
  | { readonly kind: "pairing"; readonly pairing: PairingState };

export interface Connection {
  start(): void;
  stop(): void;
  /** The stored token changed (pairing finished, or the device was forgotten). */
  tokenChanged(): void;
  subscribe(listener: (notice: ConnectionNotice) => void): () => void;
  /**
   * The user tapped to ask for pairing. Sends `device_pair_request` on the
   * pairing channel, or redials a closed one and asks once it opens; a tap
   * while a request is open sends nothing.
   */
  askToPair(): void;
  /** The user entered or scanned `code` for the open request. */
  redeem(code: string): void;
  /**
   * Send `message` to the daemon. Only a `ready` connection sends: any other
   * state drops it and answers `false`, so the screen shows the connection as
   * offline. Nothing is queued across a reconnect.
   */
  send(message: ClientMessage): boolean;
  /**
   * Forget the device token: clear this host's stored record and redial
   * without a token, landing on the pairing channel.
   */
  forget(): void;
  /** The pairing model now; each change is also a `pairing` notice. */
  pairingState(): PairingState;
  /**
   * Whether the connection is ready and presented a device token. False
   * everywhere else, and on the pairing channel, which a phone that
   * presented none lands on.
   */
  paired(): boolean;
  /**
   * Whether the connection is ready and the host listed `feature` in its
   * `Welcome`; false in every other state.
   */
  supports(feature: string): boolean;
  /** The state the connection is in right now. */
  status(): ConnectionStateName;
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
  let pairing: PairingState = INITIAL_PAIRING;
  // Whether a host already asked for a reload this visit.
  let reloadRequested = false;
  // The token the daemon issued on this page visit: the next dial presents it
  // whether or not the store kept it.
  // `null` after the daemon refused the token, while a forget is under way, or
  // after a forget whose clear failed: every dial presents no token without
  // asking the store, until a new pairing issues one.
  let issuedToken: string | null | undefined;
  const listeners = new Set<(notice: ConnectionNotice) => void>();

  function notify(notice: ConnectionNotice): void {
    for (const listener of listeners) {
      listener(notice);
    }
  }

  function foldPairing(event: PairingEvent): void {
    const next = pairingStep(pairing, event);
    if (next !== pairing) {
      pairing = next;
      notify({ kind: "pairing", pairing });
    }
  }

  /**
   * Stop presenting the token for the rest of this visit and redial without
   * one, which lands on the pairing channel. The stored record stays, so the
   * next page load presents it again.
   */
  function dropToken(): void {
    issuedToken = null;
    foldPairing({ type: "forgotten" });
    // Run after the current dispatch has finished its effects.
    void Promise.resolve().then(() => {
      dispatch({ type: "tokenChanged" });
    });
  }

  /**
   * Forget the device token: clear this host's record, then redial without a
   * token, which lands on the pairing channel. A failed clear still redials
   * without it, and keeps presenting none for the rest of the visit.
   */
  function forgetToken(): void {
    issuedToken = null;
    foldPairing({ type: "forgotten" });
    void deps.tokenStore
      .clear()
      .then(
        () => {
          if (issuedToken === null) {
            issuedToken = undefined;
          }
        },
        (error: unknown) => {
          console.warn("pwa: forgetting the device token failed", error);
        },
      )
      .then(() => {
        dispatch({ type: "tokenChanged" });
      });
  }

  function onDeviceToken(token: string): void {
    issuedToken = token;
    void deps.tokenStore.set(token).then(
      () => {
        askPersistence();
      },
      (error: unknown) => {
        console.warn("pwa: saving the device token failed", error);
        if (!stopped) {
          foldPairing({ type: "saveFailed" });
        }
      },
    );
    // Run after the current dispatch has finished its effects.
    void Promise.resolve().then(() => {
      dispatch({ type: "tokenChanged" });
    });
  }

  /**
   * Reload for a host's `reload_app`: once per page visit, since the page is
   * going anyway and a second message must not start a reload loop.
   */
  function reloadOnce(): void {
    if (reloadRequested) {
      return;
    }
    reloadRequested = true;
    deps.reloadApp();
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
    const token =
      issuedToken === undefined
        ? deps.tokenStore.get()
        : Promise.resolve(issuedToken === null ? undefined : issuedToken);
    void token.then(
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
        if (effect.message.type === "reload_app") {
          reloadOnce();
        }
        if (state.name === "pairing") {
          foldPairing({ type: "message", message: effect.message });
          if (effect.message.type === "device_token") {
            onDeviceToken(effect.message.token);
          }
        }
        break;
      case "status":
        notify({ kind: "status", state: effect.state });
        break;
      case "pairingLost":
        foldPairing({ type: "lost" });
        break;
      case "dropToken":
        dropToken();
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

  function askToPair(): void {
    // Only the pairing channel asks, and only when nothing is open already.
    if (stopped || state.name !== "pairing" || !canAsk(pairing)) {
      return;
    }
    foldPairing({ type: "ask" });
    dispatch({ type: "askPair" });
  }

  function redeem(code: string): void {
    if (stopped) {
      return;
    }
    const requestId = pairing.requestId;
    foldPairing({ type: "redeem", code });
    if (isPairingCode(code) && requestId !== null) {
      dispatch({ type: "redeem", requestId, code });
    }
  }

  function send(message: ClientMessage): boolean {
    // An unpaired connection is ready for the pairing messages only: the
    // daemon drops anything else there, so claiming a send would show success
    // for a frame that never leaves.
    if (
      stopped ||
      state.name !== "ready" ||
      state.unpaired === true ||
      socket === undefined ||
      socket.readyState !== 1
    ) {
      return false;
    }
    try {
      socket.send(JSON.stringify(message));
    } catch (error) {
      console.warn("pwa: sending a message failed", error);
      return false;
    }
    return true;
  }

  function forget(): void {
    if (!stopped) {
      forgetToken();
    }
  }

  function paired(): boolean {
    return state.name === "ready" && state.unpaired !== true;
  }

  function supports(feature: string): boolean {
    return state.name === "ready" && state.features.has(feature);
  }

  return {
    start,
    stop,
    tokenChanged,
    subscribe,
    askToPair,
    redeem,
    send,
    forget,
    pairingState: () => pairing,
    paired,
    supports,
    status: () => state.name,
  };
}

let persistenceAsked = false;

/**
 * Ask the browser to keep the page's storage through eviction, once per page
 * load. Never awaited: the redial does not wait on it, and the answer is only
 * logged.
 */
function askPersistence(): void {
  if (persistenceAsked) {
    return;
  }
  persistenceAsked = true;
  const storage = typeof navigator === "undefined" ? undefined : navigator.storage;
  const asked = storage?.persist?.();
  if (asked === undefined) {
    return;
  }
  void asked.then(
    (persisted) => {
      console.info("pwa: storage persistence granted", persisted);
    },
    (error: unknown) => {
      console.warn("pwa: asking for storage persistence failed", error);
    },
  );
}
