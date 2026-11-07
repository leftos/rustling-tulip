import { expect, test } from "vitest";

import type { NewRequestId } from "./home-actions";
import { OFFLINE_BAR } from "./connection-bar";
import type { DaemonMessage } from "./protocol";
import {
  ALLOW_LABEL,
  DENIED,
  FINISHED_LABEL,
  HOLD_AT_DESK_LABEL,
  NEEDS_YOU_LABEL,
  NOT_INSTALLED,
  UNSUPPORTED,
  foldError,
  foldStatus,
  markOffline,
  openCheck,
  panelOpen,
  permissionAnswered,
  pushScreen,
  pushState,
  setSwitch,
  subscribed,
  turnOff,
  turnOn,
  unsubscribed,
  type PushDaemon,
  type PushDevice,
  type PushState,
  type PushStep,
} from "./push-model";
import type { PushKeys } from "./push-port";

const VAPID = "BOr2-a-fake-vapid-public-key";
const KEYS: PushKeys = {
  endpoint: "https://push.example.invalid/phone",
  p256dh: "p256dh-key",
  auth: "auth-secret",
};
const OTHER = "https://push.example.invalid/other";

type PushStatusMessage = Extract<DaemonMessage, { type: "push_status" }>;
type ErrorMessage = Extract<DaemonMessage, { type: "error" }>;

function device(extra: Partial<PushDevice> = {}): PushDevice {
  return { supported: true, installed: true, permission: "granted", keys: null, ...extra };
}

function daemon(extra: Partial<PushDaemon> = {}): PushDaemon {
  return {
    vapidPublicKey: VAPID,
    endpoint: null,
    needsYou: true,
    finished: true,
    holdAtDesk: true,
    ...extra,
  };
}

/** A subscribed phone: it holds {@link KEYS} and the daemon records that endpoint. */
function onState(extra: Partial<PushDaemon> = {}): PushState {
  return pushState(device({ keys: KEYS }), daemon({ endpoint: KEYS.endpoint, ...extra }));
}

function status(extra: Partial<PushStatusMessage> = {}): PushStatusMessage {
  return {
    type: "push_status",
    vapid_public_key: VAPID,
    endpoint: null,
    needs_you: true,
    finished: true,
    request_id: null,
    ...extra,
  };
}

function error(message: string, requestId: string | null): ErrorMessage {
  return { type: "error", message, request_id: requestId };
}

/** A generator giving `r1`, `r2`, … in the order it is called. */
function counter(): NewRequestId {
  let next = 0;
  return () => {
    next += 1;
    return `r${next}`;
  };
}

function requestIdOf(step: PushStep): string {
  const message = step.message;
  if (message === null) {
    throw new Error("expected a message");
  }
  if (
    message.type === "get_push_status" ||
    message.type === "register_push" ||
    message.type === "unregister_push"
  ) {
    return message.request_id;
  }
  throw new Error(`unexpected message ${message.type}`);
}

const REGISTRATION = {
  kind: "web_push",
  endpoint: KEYS.endpoint,
  p256dh: KEYS.p256dh,
  auth: KEYS.auth,
};

test("a browser with no push API shows the unsupported text and no switch", () => {
  const screen = pushScreen(pushState(device({ supported: false })));
  expect(screen.kind).toBe("unsupported");
  expect(screen.message).toBe(UNSUPPORTED);
  expect(screen.allow).toBeNull();
  expect(screen.needsYou).toBeNull();
  expect(screen.finished).toBeNull();
});

test("a page outside the Home Screen shows the install steps and no switch", () => {
  const screen = pushScreen(pushState(device({ installed: false })));
  expect(screen.kind).toBe("not-installed");
  expect(screen.message).toBe(NOT_INSTALLED);
  expect(screen.allow).toBeNull();
});

test("a blocked permission shows the Settings text and no switch", () => {
  const screen = pushScreen(pushState(device({ permission: "denied" })));
  expect(screen.kind).toBe("denied");
  expect(screen.message).toBe(DENIED);
  expect(screen.allow).toBeNull();
});

test("with no subscription the panel shows the main switch alone", () => {
  const waiting = pushScreen(pushState(device()));
  expect(waiting.kind).toBe("off");
  expect(waiting.message).toBeNull();
  expect(waiting.allow).toEqual({ label: ALLOW_LABEL, on: false, enabled: false });
  expect(waiting.needsYou).toBeNull();
  expect(waiting.finished).toBeNull();

  const ready = pushScreen(pushState(device(), daemon()));
  expect(ready.allow).toEqual({ label: ALLOW_LABEL, on: false, enabled: true });
});

test("a subscribed phone shows the three switches with the daemon's values", () => {
  const screen = pushScreen(onState({ needsYou: false, finished: true }));
  expect(screen.kind).toBe("on");
  expect(screen.allow).toEqual({ label: ALLOW_LABEL, on: true, enabled: true });
  expect(screen.needsYou).toEqual({ label: NEEDS_YOU_LABEL, on: false, enabled: true });
  expect(screen.finished).toEqual({ label: FINISHED_LABEL, on: true, enabled: true });
});

test("a request in flight disables every switch", () => {
  const check = openCheck(onState(), counter());
  const screen = pushScreen(check.state);
  expect(screen.allow?.enabled).toBe(false);
  expect(screen.needsYou?.enabled).toBe(false);
  expect(screen.finished?.enabled).toBe(false);
});

test("the daemon's refusal keeps the state and shows its text verbatim", () => {
  const check = openCheck(pushState(device()), counter());
  const refused = foldError(
    check.state,
    error("this computer cannot send notifications", requestIdOf(check)),
  );
  expect(refused.state.pending).toBeNull();
  expect(refused.state.error).toBe("this computer cannot send notifications");
  expect(refused.effects).toEqual([]);
  expect(pushScreen(refused.state).kind).toBe("off");
});

test("an error for another request leaves the panel alone", () => {
  const check = openCheck(pushState(device()), counter());
  const other = foldError(check.state, error("not ours", "another-request"));
  expect(other.state).toBe(check.state);
  expect(other.effects).toEqual([]);
});

test("a status answer for another request records the daemon and leaves the request out", () => {
  const check = openCheck(pushState(device()), counter());
  const folded = foldStatus(
    check.state,
    status({ request_id: "someone-else", endpoint: OTHER }),
    counter(),
  );
  expect(folded.message).toBeNull();
  expect(folded.effects).toEqual([]);
  expect(folded.state.daemon?.endpoint).toBe(OTHER);
  expect(folded.state.pending).toEqual(check.state.pending);
});

test("a reply never clears the browser's work", () => {
  const turned = turnOn(pushState(device(), daemon()));
  const folded = foldStatus(turned.state, status({ request_id: null }), counter());
  expect(folded.state.pending).toEqual({ kind: "subscribe", requestId: null });
  expect(folded.state.daemon?.vapidPublicKey).toBe(VAPID);
});

test("a first enable subscribes with the daemon's key and registers both switches on", () => {
  const ids = counter();
  const start = pushState(device(), daemon());
  const turned = turnOn(start);
  expect(turned.message).toBeNull();
  expect(turned.effects).toEqual([{ kind: "subscribe", vapidPublicKey: VAPID }]);

  const done = subscribed(turned.state, KEYS, ids);
  expect(done.message).toEqual({
    type: "register_push",
    registration: REGISTRATION,
    needs_you: true,
    finished: true,
    hold_at_desk: true,
    request_id: "r1",
  });
  expect(pushScreen(done.state).kind).toBe("on");
});

test("an undecided permission asks first and a granted answer goes on to subscribe", () => {
  const turned = turnOn(pushState(device({ permission: "default" }), daemon()));
  expect(turned.effects).toEqual([{ kind: "ask-permission" }]);
  const granted = permissionAnswered(turned.state, "granted");
  expect(granted.effects).toEqual([{ kind: "subscribe", vapidPublicKey: VAPID }]);
  const denied = permissionAnswered(turned.state, "denied");
  expect(denied.effects).toEqual([]);
  expect(pushScreen(denied.state).kind).toBe("denied");
});

test("a register refused after a subscribe drops the browser's subscription", () => {
  const ids = counter();
  const turned = turnOn(pushState(device(), daemon()));
  const subscribedStep = subscribed(turned.state, KEYS, ids);
  const refused = foldError(subscribedStep.state, error("refused", requestIdOf(subscribedStep)));
  expect(refused.effects).toEqual([{ kind: "unsubscribe" }]);
  const dropped = unsubscribed(refused.state);
  expect(dropped.device.keys).toBeNull();
  expect(pushScreen(dropped).kind).toBe("off");
});

test("a switch tap re-sends the register with the phone's keys and both values", () => {
  const ids = counter();
  const tapped = setSwitch(onState({ needsYou: false, finished: true }), "needsYou", true, ids);
  expect(tapped.message).toEqual({
    type: "register_push",
    registration: REGISTRATION,
    needs_you: true,
    finished: true,
    hold_at_desk: true,
    request_id: "r1",
  });
});

test("a switch tap shows its request while it is out and reverts on a refusal", () => {
  const ids = counter();
  const tapped = setSwitch(onState({ needsYou: false, finished: true }), "needsYou", true, ids);
  expect(tapped.state.daemon?.needsYou).toBe(false);
  expect(pushScreen(tapped.state).needsYou?.on).toBe(true);
  expect(pushScreen(tapped.state).needsYou?.enabled).toBe(false);

  const refused = foldError(tapped.state, error("refused", requestIdOf(tapped)));
  expect(refused.effects).toEqual([]);
  expect(pushScreen(refused.state).needsYou?.on).toBe(false);
});

test("a switch tap's refusal keeps the subscription", () => {
  const ids = counter();
  const tapped = setSwitch(onState(), "finished", false, ids);
  const refused = foldError(tapped.state, error("refused", requestIdOf(tapped)));
  expect(refused.effects).toEqual([]);
  expect(refused.state.device.keys).toEqual(KEYS);
});

test("the open check re-registers a different endpoint with the daemon's switches", () => {
  const ids = counter();
  const check = openCheck(onState(), ids);
  const folded = foldStatus(
    check.state,
    status({ endpoint: OTHER, needs_you: false, finished: true, request_id: requestIdOf(check) }),
    ids,
  );
  expect(folded.message).toEqual({
    type: "register_push",
    registration: REGISTRATION,
    needs_you: false,
    finished: true,
    hold_at_desk: true,
    request_id: "r2",
  });
});

test("the open check drops a subscription the daemon does not hold", () => {
  const ids = counter();
  const check = openCheck(onState(), ids);
  const folded = foldStatus(check.state, status({ request_id: requestIdOf(check) }), ids);
  expect(folded.message).toBeNull();
  expect(folded.effects).toEqual([{ kind: "unsubscribe" }]);
  expect(folded.state.pending).toEqual({ kind: "unsubscribe", requestId: null });
  expect(pushScreen(unsubscribed(folded.state)).kind).toBe("off");
});

test("the open check leaves a phone without a subscription to the daemon's record", () => {
  const ids = counter();
  const check = openCheck(pushState(device()), ids);
  const folded = foldStatus(
    check.state,
    status({ endpoint: OTHER, request_id: requestIdOf(check) }),
    ids,
  );
  expect(folded.message).toBeNull();
  expect(folded.effects).toEqual([]);
  expect(folded.state.daemon?.endpoint).toBe(OTHER);
  expect(folded.state.pending).toBeNull();
  expect(pushScreen(folded.state).kind).toBe("off");
});

test("a status query from an opened panel only records the daemon's answer", () => {
  const ids = counter();
  const opened = panelOpen(pushState(device()), ids);
  const folded = foldStatus(
    opened.state,
    status({ endpoint: OTHER, request_id: requestIdOf(opened) }),
    ids,
  );
  expect(folded.message).toBeNull();
  expect(folded.state.daemon?.endpoint).toBe(OTHER);
  expect(folded.state.pending).toBeNull();
});

test("the panel asks for the status only while it holds none", () => {
  const asked = panelOpen(pushState(device()), counter());
  expect(asked.message?.type).toBe("get_push_status");
  expect(panelOpen(pushState(device(), daemon()), counter()).message).toBeNull();
  expect(panelOpen(asked.state, counter()).message).toBeNull();
});

test("turning off keeps every switch disabled until the browser drops the subscription", () => {
  const ids = counter();
  const off = turnOff(onState(), ids);
  expect(off.message).toEqual({ type: "unregister_push", request_id: "r1" });
  const folded = foldStatus(
    off.state,
    status({
      request_id: requestIdOf(off),
      endpoint: KEYS.endpoint,
      needs_you: false,
      finished: false,
    }),
    ids,
  );
  expect(folded.effects).toEqual([{ kind: "unsubscribe" }]);
  expect(folded.state.pending).toEqual({ kind: "unsubscribe", requestId: null });
  expect(pushScreen(folded.state).allow?.enabled).toBe(false);
  expect(pushScreen(folded.state).kind).toBe("on");

  const done = unsubscribed(folded.state);
  expect(pushScreen(done).kind).toBe("off");
  expect(pushScreen(done).allow?.enabled).toBe(true);
});

test("a send the connection dropped is out of flight and says so", () => {
  const check = openCheck(pushState(device()), counter());
  const offline = markOffline(check.state);
  expect(offline.pending).toBeNull();
  expect(pushScreen(offline).error).toBe(OFFLINE_BAR);
});

test("a status without hold_at_desk reads on", () => {
  const ids = counter();
  const check = openCheck(onState(), ids);
  const folded = foldStatus(
    check.state,
    status({ endpoint: KEYS.endpoint, request_id: requestIdOf(check) }),
    ids,
  );
  expect(folded.message).toBeNull();
  expect(folded.state.daemon?.holdAtDesk).toBe(true);
  expect(pushScreen(folded.state).holdAtDesk?.on).toBe(true);
});

test("a first enable registers hold_at_desk on", () => {
  const ids = counter();
  const turned = turnOn(pushState(device(), daemon({ holdAtDesk: false })));
  const done = subscribed(turned.state, KEYS, ids);
  expect(done.message).toEqual({
    type: "register_push",
    registration: REGISTRATION,
    needs_you: true,
    finished: true,
    hold_at_desk: true,
    request_id: "r1",
  });
});

test("tapping hold_at_desk registers it and keeps the other switches", () => {
  const ids = counter();
  const tapped = setSwitch(
    onState({ needsYou: false, finished: true, holdAtDesk: true }),
    "holdAtDesk",
    false,
    ids,
  );
  expect(tapped.message).toEqual({
    type: "register_push",
    registration: REGISTRATION,
    needs_you: false,
    finished: true,
    hold_at_desk: false,
    request_id: "r1",
  });
  expect(pushScreen(tapped.state).holdAtDesk?.on).toBe(false);
  expect(pushScreen(tapped.state).holdAtDesk?.enabled).toBe(false);
});

test("the open check re-registers with the daemon's hold_at_desk", () => {
  const ids = counter();
  const check = openCheck(onState(), ids);
  const folded = foldStatus(
    check.state,
    status({
      endpoint: OTHER,
      needs_you: false,
      finished: true,
      hold_at_desk: false,
      request_id: requestIdOf(check),
    }),
    ids,
  );
  expect(folded.message).toEqual({
    type: "register_push",
    registration: REGISTRATION,
    needs_you: false,
    finished: true,
    hold_at_desk: false,
    request_id: "r2",
  });
});

test("the screen shows hold_at_desk after finished only when on", () => {
  const off = pushScreen(pushState(device(), daemon()));
  expect(off.kind).toBe("off");
  expect(off.holdAtDesk).toBeNull();

  const on = pushScreen(onState({ finished: false, holdAtDesk: false }));
  expect(on.kind).toBe("on");
  expect(on.holdAtDesk).toEqual({ label: HOLD_AT_DESK_LABEL, on: false, enabled: true });
});
