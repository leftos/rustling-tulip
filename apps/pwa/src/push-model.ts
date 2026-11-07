import type { NewRequestId } from "./home-actions";
import { OFFLINE_BAR } from "./home-screen";
import type { ClientMessage, DaemonMessage } from "./protocol";
import type { PushKeys, PushPermission } from "./push-port";

/** What the panel says when the browser has no push API at all. */
export const UNSUPPORTED = "This browser can't receive notifications.";

/** What it says when the page runs in a tab rather than from the Home Screen, which iOS requires. */
export const NOT_INSTALLED =
  "Add Rustling Tulip to your Home Screen to get notifications: tap Share, then Add to Home Screen.";

/** What it says when the permission is blocked: only the phone's Settings can undo that. */
export const DENIED =
  "Notifications are blocked for this app. Turn them on in Settings, then come back.";

/** The panel's heading. */
export const HEADING = "Notifications";

/** The label of the button that closes the panel. */
export const DONE = "Done";

/** The main switch: whether this phone receives notifications. */
export const ALLOW_LABEL = "Allow notifications";

/** The switch that pushes the sessions waiting on the user. */
export const NEEDS_YOU_LABEL = "Needs you";

/** The switch that pushes finished sessions. */
export const FINISHED_LABEL = "Finished";

/** The switch that holds pushes while the owner is at their computer. */
export const HOLD_AT_DESK_LABEL = "Hold while I'm at my computer";

/** The three switches beside the main one, by the field they set. */
export type PushSwitchName = "needsYou" | "finished" | "holdAtDesk";

/** What the phone's browser offers, as the panel reads it from the port. */
export interface PushDevice {
  readonly supported: boolean;
  readonly installed: boolean;
  readonly permission: PushPermission;
  /** The subscription the phone holds; null when it holds none. */
  readonly keys: PushKeys | null;
}

/** The daemon's record for this phone, from its last `push_status`. */
export interface PushDaemon {
  /** The key `subscribe` is handed, base64url; the daemon makes it on the first ask. */
  readonly vapidPublicKey: string;
  /** The endpoint the daemon holds for this phone; null when it holds none. */
  readonly endpoint: string | null;
  readonly needsYou: boolean;
  readonly finished: boolean;
  readonly holdAtDesk: boolean;
}

/** The three switches a register carries. */
export interface PushSwitches {
  readonly needsYou: boolean;
  readonly finished: boolean;
  readonly holdAtDesk: boolean;
}

/**
 * The work out for the panel, whether on the wire or in the browser: a message
 * with the request id it answers by, a register with the switches it asked
 * for, or browser work, which carries no request id.
 */
export type PushPending =
  | { readonly kind: "check" | "status"; readonly requestId: string }
  | { readonly kind: "permission" | "subscribe"; readonly requestId: null }
  | {
      readonly kind: "register";
      readonly requestId: string;
      /** True when the register followed a subscribe this page made, so its refusal drops the subscription. */
      readonly fresh: boolean;
      /** The switches the register asked for, which the panel shows until the reply. */
      readonly switches: PushSwitches;
    }
  | { readonly kind: "unregister"; readonly requestId: string }
  | { readonly kind: "unsubscribe"; readonly requestId: null };

/** What the panel keeps between taps and replies. */
export interface PushState {
  readonly device: PushDevice;
  /** The daemon's last record; null before the first `push_status`. */
  readonly daemon: PushDaemon | null;
  readonly pending: PushPending | null;
  /** The line under the switches: the daemon's refusal verbatim, or {@link OFFLINE_BAR}. */
  readonly error: string | null;
}

/** Browser work the view runs through the port, inside the tap's own task. */
export type PushEffect =
  | { readonly kind: "ask-permission" }
  | { readonly kind: "subscribe"; readonly vapidPublicKey: string }
  | { readonly kind: "unsubscribe" };

/** What a tap or a reply produces: the message to send, the browser work to run, and the state after. */
export interface PushStep {
  readonly message: ClientMessage | null;
  readonly effects: readonly PushEffect[];
  readonly state: PushState;
}

/** One switch of the panel: its label, where it stands and whether a tap changes it. */
export interface PushSwitch {
  readonly label: string;
  readonly on: boolean;
  readonly enabled: boolean;
}

/** Everything the panel draws. */
export interface PushScreen {
  readonly kind: "unsupported" | "not-installed" | "denied" | "off" | "on";
  readonly heading: string;
  readonly done: string;
  /** The line under the heading; null when the switches speak for themselves. */
  readonly message: string | null;
  /** The main switch; null in the states that cannot change anything. */
  readonly allow: PushSwitch | null;
  /** The three switches; null unless the phone is subscribed. */
  readonly needsYou: PushSwitch | null;
  readonly finished: PushSwitch | null;
  readonly holdAtDesk: PushSwitch | null;
  readonly error: string | null;
}

/** The state a visit starts from: the browser as the port reports it and nothing from the daemon yet. */
export function pushState(device: PushDevice, daemon: PushDaemon | null = null): PushState {
  return { device, daemon, pending: null, error: null };
}

/** The state after the port reported the browser afresh, folded into the state as it stands now. */
export function deviceRead(state: PushState, device: PushDevice): PushState {
  return { ...state, device };
}

/**
 * What the panel shows: the state text for a browser or permission that
 * cannot push, else the switches — the main one alone while the phone holds no
 * subscription, all of them once it does. While a register is out the three
 * switches show what it asked for, and a reply that refuses it leaves the
 * daemon's earlier values.
 */
export function pushScreen(state: PushState): PushScreen {
  const pending = state.pending;
  const requested = pending !== null && pending.kind === "register" ? pending.switches : null;
  const base = { heading: HEADING, done: DONE, error: state.error };
  if (!state.device.supported) {
    return {
      ...base,
      kind: "unsupported",
      message: UNSUPPORTED,
      allow: null,
      needsYou: null,
      finished: null,
      holdAtDesk: null,
    };
  }
  if (!state.device.installed) {
    return {
      ...base,
      kind: "not-installed",
      message: NOT_INSTALLED,
      allow: null,
      needsYou: null,
      finished: null,
      holdAtDesk: null,
    };
  }
  if (state.device.permission === "denied") {
    return {
      ...base,
      kind: "denied",
      message: DENIED,
      allow: null,
      needsYou: null,
      finished: null,
      holdAtDesk: null,
    };
  }
  const busy = pending !== null;
  const subscribed = state.device.keys !== null;
  const allow: PushSwitch = {
    label: ALLOW_LABEL,
    on: subscribed,
    // Subscribing needs the daemon's VAPID key, which only a status carries.
    enabled: !busy && (subscribed || state.daemon !== null),
  };
  if (!subscribed) {
    return {
      ...base,
      kind: "off",
      message: null,
      allow,
      needsYou: null,
      finished: null,
      holdAtDesk: null,
    };
  }
  return {
    ...base,
    kind: "on",
    message: null,
    allow,
    needsYou: {
      label: NEEDS_YOU_LABEL,
      on: requested?.needsYou ?? state.daemon?.needsYou ?? true,
      enabled: !busy,
    },
    finished: {
      label: FINISHED_LABEL,
      on: requested?.finished ?? state.daemon?.finished ?? true,
      enabled: !busy,
    },
    holdAtDesk: {
      label: HOLD_AT_DESK_LABEL,
      on: requested?.holdAtDesk ?? state.daemon?.holdAtDesk ?? true,
      enabled: !busy,
    },
  };
}

/**
 * The check on app open: asks the daemon for its push state. The reply
 * re-registers this phone only when both sides hold an endpoint and the two
 * differ, and drops a subscription the daemon no longer holds; it never
 * prompts or subscribes, since iOS needs a tap for both.
 */
export function openCheck(state: PushState, newRequestId: NewRequestId): PushStep {
  return statusRequest(state, newRequestId, "check");
}

/**
 * The panel was opened with no `push_status` held yet (the open check failed,
 * or has not answered): ask again. This records the daemon's answer only; the
 * check's re-register stays with the open check.
 */
export function panelOpen(state: PushState, newRequestId: NewRequestId): PushStep {
  if (state.daemon !== null || state.pending !== null) {
    return none(state);
  }
  const step = statusRequest(state, newRequestId, "status");
  // The daemon's last answer stands until this one arrives, refusal included.
  return { ...step, state: { ...step.state, error: state.error } };
}

/**
 * One daemon message folded into the panel. A `push_status` records the
 * daemon's keys, endpoint and switches whatever request it answers; only the
 * reply whose `request_id` is the one out ends that request, so a reply to
 * something else never clears the work in flight. A check's answer
 * re-registers differing endpoints with the daemon's switches and drops a
 * subscription the daemon does not hold; an unregister's answer hands the
 * browser its unsubscribe. Pure; a message the panel does not read returns the
 * state itself.
 */
export function foldStatus(
  state: PushState,
  message: Extract<DaemonMessage, { type: "push_status" }>,
  newRequestId: NewRequestId,
): PushStep {
  const daemon: PushDaemon = {
    vapidPublicKey: message.vapid_public_key,
    endpoint: message.endpoint ?? null,
    needsYou: message.needs_you,
    finished: message.finished,
    holdAtDesk: message.hold_at_desk ?? true,
  };
  const pending = state.pending;
  if (
    pending === null ||
    pending.requestId === null ||
    (message.request_id ?? null) !== pending.requestId
  ) {
    return none({ ...state, daemon });
  }
  const next: PushState = { ...state, daemon, pending: null, error: null };
  if (pending.kind === "unregister") {
    return unsubscribe(next);
  }
  if (pending.kind === "check") {
    const keys = state.device.keys;
    // iOS can report no subscription while pushes still arrive: the daemon keeps its record.
    if (keys === null) {
      return none(next);
    }
    // The daemon lost the phone's subscription (a failed turn-off, or a lost folder).
    if (daemon.endpoint === null) {
      return unsubscribe(next);
    }
    if (keys.endpoint !== daemon.endpoint) {
      return register(
        next,
        keys,
        {
          needsYou: daemon.needsYou,
          finished: daemon.finished,
          holdAtDesk: daemon.holdAtDesk,
        },
        false,
        newRequestId,
      );
    }
  }
  return none(next);
}

/**
 * One refusal folded into the panel: an error echoing the request out ends it
 * and keeps the daemon's text verbatim, leaving the state it was in. A fresh
 * register's refusal also drops the browser's subscription, which the daemon
 * does not hold. Pure; an error for no request of this panel, or for browser
 * work that carries no request id, returns the state.
 */
export function foldError(
  state: PushState,
  message: Extract<DaemonMessage, { type: "error" }>,
): PushStep {
  const pending = state.pending;
  if (pending === null || pending.requestId === null) {
    return none(state);
  }
  if ((message.request_id ?? null) !== pending.requestId) {
    return none(state);
  }
  const fresh = pending.kind === "register" && pending.fresh;
  return {
    message: null,
    effects: fresh ? [{ kind: "unsubscribe" }] : [],
    state: { ...state, pending: null, error: message.message },
  };
}

/**
 * The main switch turned on, from a tap: the permission is asked for inside the
 * tap when the phone has not decided, then the browser subscribes with the
 * daemon's VAPID key. With no `push_status` held there is no key, so a tap sends
 * nothing; the panel shows the switch disabled in that state.
 */
export function turnOn(state: PushState): PushStep {
  if (state.pending !== null || state.device.keys !== null) {
    return none(state);
  }
  const daemon = state.daemon;
  if (daemon === null || state.device.permission === "denied") {
    return none(state);
  }
  if (state.device.permission === "default") {
    return {
      message: null,
      effects: [{ kind: "ask-permission" }],
      state: { ...state, pending: { kind: "permission", requestId: null }, error: null },
    };
  }
  return subscribeWith({ ...state, error: null }, daemon.vapidPublicKey);
}

/** What the permission prompt answered: granted goes on to subscribe, anything else leaves the panel. */
export function permissionAnswered(state: PushState, permission: PushPermission): PushStep {
  const device: PushDevice = { ...state.device, permission };
  const next: PushState = { ...state, device, pending: null };
  if (permission !== "granted" || state.daemon === null) {
    return none(next);
  }
  return subscribeWith(next, state.daemon.vapidPublicKey);
}

/**
 * What the browser's subscribe produced: the phone holds `keys` now, so the
 * daemon is told about it with both switches on a first enable, and with the
 * ones it already holds otherwise.
 */
export function subscribed(state: PushState, keys: PushKeys, newRequestId: NewRequestId): PushStep {
  const daemon = state.daemon;
  const first = daemon === null || daemon.endpoint === null;
  const switches: PushSwitches = {
    needsYou: first || daemon.needsYou,
    finished: first || daemon.finished,
    holdAtDesk: first || daemon.holdAtDesk,
  };
  const next: PushState = {
    ...state,
    device: { ...state.device, keys },
    pending: null,
    error: null,
  };
  return register(next, keys, switches, true, newRequestId);
}

/** The main switch turned off: asks the daemon to forget the phone; its answer drops the browser's subscription. */
export function turnOff(state: PushState, newRequestId: NewRequestId): PushStep {
  if (state.pending !== null || state.device.keys === null) {
    return none(state);
  }
  const requestId = newRequestId();
  return {
    message: { type: "unregister_push", request_id: requestId },
    effects: [],
    state: { ...state, pending: { kind: "unregister", requestId }, error: null },
  };
}

/** The browser dropped the subscription, as the state shows from now on. */
export function unsubscribed(state: PushState): PushState {
  return { ...state, device: { ...state.device, keys: null }, pending: null };
}

/**
 * One of the three switches tapped: re-sends the register with the phone's keys
 * and all three values. The daemon's record keeps its values until its reply,
 * so a refusal leaves the earlier ones.
 */
export function setSwitch(
  state: PushState,
  which: PushSwitchName,
  value: boolean,
  newRequestId: NewRequestId,
): PushStep {
  const keys = state.device.keys;
  const daemon = state.daemon;
  if (state.pending !== null || keys === null || daemon === null) {
    return none(state);
  }
  const switches: PushSwitches = {
    needsYou: which === "needsYou" ? value : daemon.needsYou,
    finished: which === "finished" ? value : daemon.finished,
    holdAtDesk: which === "holdAtDesk" ? value : daemon.holdAtDesk,
  };
  return register(state, keys, switches, false, newRequestId);
}

/** A send the connection dropped: nothing is out any more, and the panel says why with the Home bar's word. */
export function markOffline(state: PushState): PushState {
  return { ...state, pending: null, error: OFFLINE_BAR };
}

/** The browser side threw: nothing is out any more, and the panel shows the thrown text. */
export function markFailed(state: PushState, text: string): PushState {
  return { ...state, pending: null, error: text };
}

/** A step that sends nothing and does nothing. */
function none(state: PushState): PushStep {
  return { message: null, effects: [], state };
}

/** The browser unsubscribe that follows an unregister's answer, with the switches off until it settles. */
function unsubscribe(state: PushState): PushStep {
  return {
    message: null,
    effects: [{ kind: "unsubscribe" }],
    state: { ...state, pending: { kind: "unsubscribe", requestId: null } },
  };
}

/** The `get_push_status` a check or a panel open sends. */
function statusRequest(
  state: PushState,
  newRequestId: NewRequestId,
  kind: "check" | "status",
): PushStep {
  const requestId = newRequestId();
  return {
    message: { type: "get_push_status", request_id: requestId },
    effects: [],
    state: { ...state, pending: { kind, requestId }, error: null },
  };
}

/** The `register_push` for `keys` with the three switches the panel asked for. */
function register(
  state: PushState,
  keys: PushKeys,
  switches: PushSwitches,
  fresh: boolean,
  newRequestId: NewRequestId,
): PushStep {
  const requestId = newRequestId();
  return {
    message: {
      type: "register_push",
      registration: {
        kind: "web_push",
        endpoint: keys.endpoint,
        p256dh: keys.p256dh,
        auth: keys.auth,
      },
      needs_you: switches.needsYou,
      finished: switches.finished,
      hold_at_desk: switches.holdAtDesk,
      request_id: requestId,
    },
    effects: [],
    state: { ...state, pending: { kind: "register", requestId, fresh, switches }, error: null },
  };
}

/** The browser subscribe the view runs inside the tap's own task. */
function subscribeWith(state: PushState, vapidPublicKey: string): PushStep {
  return {
    message: null,
    effects: [{ kind: "subscribe", vapidPublicKey }],
    state: { ...state, pending: { kind: "subscribe", requestId: null }, error: null },
  };
}
