import { expect, test } from "vitest";

import { mountNotificationsView } from "./notifications-view";
import { DONE, UNSUPPORTED } from "./push-model";
import {
  FAKE_KEYS,
  memoryPushPort,
  type MemoryPushPort,
  type PushKeys,
  type PushPort,
} from "./push-port";
import type { ClientMessage, DaemonMessage } from "./protocol";
import type { Connection, ConnectionNotice } from "./socket";

const VAPID = "BOr2-a-fake-vapid-public-key";
const KEYS: PushKeys = {
  endpoint: "https://push.example.invalid/phone",
  p256dh: "p256dh-key",
  auth: "auth-secret",
};
const OTHER = "https://push.example.invalid/other";

/** The few node calls the panel makes, and the handler a test fires. */
class FakeNode {
  className = "";
  textContent = "";
  type = "";
  checked = false;
  disabled = false;
  hidden = false;
  readonly children: FakeNode[] = [];
  private readonly handlers = new Map<string, (() => void)[]>();

  append(...nodes: FakeNode[]): void {
    this.children.push(...nodes);
  }

  replaceChildren(...nodes: FakeNode[]): void {
    this.children.splice(0, this.children.length, ...nodes);
  }

  addEventListener(kind: string, handler: () => void): void {
    const held = this.handlers.get(kind) ?? [];
    held.push(handler);
    this.handlers.set(kind, held);
  }

  removeEventListener(kind: string, handler: () => void): void {
    const held = this.handlers.get(kind) ?? [];
    this.handlers.set(
      kind,
      held.filter((known) => known !== handler),
    );
  }

  fire(kind: string): void {
    for (const handler of this.handlers.get(kind) ?? []) {
      handler();
    }
  }

  /** Every descendant, depth first, in drawing order. */
  descendants(): FakeNode[] {
    const found: FakeNode[] = [];
    for (const child of this.children) {
      found.push(child, ...child.descendants());
    }
    return found;
  }
}

class FakeDocument {
  createElement(): FakeNode {
    return new FakeNode();
  }
}

class FakeWindow {
  private readonly handlers = new Map<string, (() => void)[]>();

  addEventListener(kind: string, handler: () => void): void {
    const held = this.handlers.get(kind) ?? [];
    held.push(handler);
    this.handlers.set(kind, held);
  }

  removeEventListener(kind: string, handler: () => void): void {
    const held = this.handlers.get(kind) ?? [];
    this.handlers.set(
      kind,
      held.filter((known) => known !== handler),
    );
  }

  fire(kind: string): void {
    for (const handler of this.handlers.get(kind) ?? []) {
      handler();
    }
  }
}

// The page's views are written against the DOM; this file runs in node, so it
// gives them the little of it the panel draws with.
const fakeWindow = new FakeWindow();
const host = globalThis as unknown as { document: FakeDocument; window: FakeWindow };
host.document = new FakeDocument();
host.window = fakeWindow;

interface Harness {
  readonly section: FakeNode;
  readonly button: FakeNode;
  /** Every message the panel sent, in order. */
  readonly sent: ClientMessage[];
  notice(notice: ConnectionNotice): void;
}

/** The panel over a scripted connection whose sends all land. */
function harness(port: PushPort = memoryPushPort()): Harness {
  const section = new FakeNode();
  const button = new FakeNode();
  const sent: ClientMessage[] = [];
  const listeners: ((notice: ConnectionNotice) => void)[] = [];
  const connection = {
    subscribe(listener: (notice: ConnectionNotice) => void) {
      listeners.push(listener);
      return () => {};
    },
    send(message: ClientMessage) {
      sent.push(message);
      return true;
    },
    paired: () => true,
    status: () => "ready",
  } as unknown as Connection;
  mountNotificationsView(
    section as unknown as HTMLElement,
    button as unknown as HTMLElement,
    connection,
    port,
  );
  return {
    section,
    button,
    sent,
    notice(notice) {
      for (const listener of listeners) {
        listener(notice);
      }
    },
  };
}

/** A port whose browser calls throw when the panel reads them without the push API. */
function guarded(port: MemoryPushPort): MemoryPushPort {
  return {
    ...port,
    permission: () => {
      if (!port.supported()) {
        throw new Error("Notification is not defined");
      }
      return port.permission();
    },
    current: () => {
      if (!port.supported()) {
        throw new Error("PushManager is not defined");
      }
      return port.current();
    },
  };
}

/** Lets every promise the panel started settle before the assertions. */
async function flush(): Promise<void> {
  await new Promise<void>((resolve) => {
    setTimeout(resolve, 0);
  });
}

function requestIdOf(message: ClientMessage | undefined): string | null {
  if (message === undefined) {
    return null;
  }
  if (
    message.type === "get_push_status" ||
    message.type === "register_push" ||
    message.type === "unregister_push"
  ) {
    return message.request_id;
  }
  return null;
}

/** The daemon's answer to a status ask: its key, the endpoint it holds, and the switches. */
function status(
  requestId: string | null,
  endpoint: string | null = null,
  needsYou = true,
): DaemonMessage {
  return {
    type: "push_status",
    vapid_public_key: VAPID,
    endpoint,
    needs_you: needsYou,
    finished: true,
    request_id: requestId,
  };
}

function checkboxes(node: FakeNode): FakeNode[] {
  return node.descendants().filter((child) => child.type === "checkbox");
}

function failureText(node: FakeNode): string | undefined {
  return node.descendants().find((child) => child.className === "notifications-error")?.textContent;
}

test("the Notifications button opens the panel and Done hides it", async () => {
  const h = harness();
  h.notice({ kind: "status", state: "ready" });
  await flush();
  expect(h.button.hidden).toBe(false);
  expect(h.section.hidden).toBe(true);

  h.button.fire("click");
  await flush();
  expect(h.section.hidden).toBe(false);

  const done = h.section.descendants().find((node) => node.textContent === DONE);
  expect(done).toBeDefined();
  done?.fire("click");
  expect(h.section.hidden).toBe(true);
});

test("a browser without push mounts and reads nothing it does not have", async () => {
  const h = harness(guarded(memoryPushPort({ supported: false })));
  h.notice({ kind: "status", state: "ready" });
  await flush();
  h.button.fire("click");
  await flush();

  const text = h.section.descendants().find((node) => node.textContent === UNSUPPORTED);
  expect(text).toBeDefined();
});

test("a device read that fails shows its message instead of crashing", async () => {
  const base = memoryPushPort();
  const failing: PushPort = {
    ...base,
    current: () => Promise.reject(new Error("no service worker")),
  };
  const h = harness(failing);
  h.notice({ kind: "status", state: "ready" });
  await flush();
  h.button.fire("click");
  await flush();

  expect(failureText(h.section)).toBe("no service worker");
});

test("a device read that resolves late keeps the daemon's answer", async () => {
  const base = memoryPushPort({ keys: KEYS });
  let release: (keys: PushKeys | null) => void = () => {};
  const held = new Promise<PushKeys | null>((resolve) => {
    release = resolve;
  });
  const h = harness({ ...base, current: () => held });
  h.notice({ kind: "status", state: "ready" });
  h.notice({ kind: "message", message: status("someone-else", OTHER, false) });
  release(KEYS);
  await flush();

  h.button.fire("click");
  await flush();
  // The daemon said "Needs you" is off before the phone's subscription arrived;
  // the late reading must not drop that answer.
  expect(checkboxes(h.section)[1]?.checked).toBe(false);
});

test("the main switch subscribes with the daemon's key and registers both switches", async () => {
  const port = memoryPushPort();
  const h = harness(port);
  h.notice({ kind: "status", state: "ready" });
  await flush();
  h.notice({ kind: "message", message: status(requestIdOf(h.sent[0])) });

  h.button.fire("click");
  await flush();
  const box = checkboxes(h.section)[0];
  expect(box?.disabled).toBe(false);
  box?.fire("change");
  await flush();

  expect(port.asked).toEqual([VAPID]);
  expect(h.sent.at(-1)).toEqual({
    type: "register_push",
    registration: {
      kind: "web_push",
      endpoint: FAKE_KEYS.endpoint,
      p256dh: FAKE_KEYS.p256dh,
      auth: FAKE_KEYS.auth,
    },
    needs_you: true,
    finished: true,
    request_id: expect.any(String),
  });
});

test("opening the panel with no status held asks for it again", async () => {
  const h = harness();
  h.notice({ kind: "status", state: "ready" });
  await flush();
  h.notice({
    kind: "message",
    message: {
      type: "error",
      message: "not supported",
      request_id: requestIdOf(h.sent[0]),
      code: "unsupported_message",
    },
  });

  h.button.fire("click");
  await flush();
  expect(h.sent.at(-1)?.type).toBe("get_push_status");
  expect(failureText(h.section)).toBe("not supported");
});

test("a disconnect clears the request in flight", async () => {
  const port = memoryPushPort({ keys: KEYS });
  const h = harness(port);
  h.notice({ kind: "status", state: "ready" });
  await flush();
  h.notice({ kind: "message", message: status(requestIdOf(h.sent[0]), KEYS.endpoint) });

  h.button.fire("click");
  await flush();
  const boxes = checkboxes(h.section);
  expect(boxes.length).toBe(3);
  boxes[1]?.fire("change");
  await flush();
  expect(checkboxes(h.section)[1]?.disabled).toBe(true);

  h.notice({ kind: "status", state: "backoff" });
  expect(checkboxes(h.section)[1]?.disabled).toBe(false);
  expect(failureText(h.section)).toBe("Offline");
});

test("the panel hides when Home hides and on popstate", async () => {
  const h = harness();
  h.notice({ kind: "status", state: "ready" });
  await flush();
  h.button.fire("click");
  await flush();
  expect(h.section.hidden).toBe(false);

  h.notice({ kind: "status", state: "pairing" });
  expect(h.section.hidden).toBe(true);

  h.notice({ kind: "status", state: "ready" });
  await flush();
  h.button.fire("click");
  await flush();
  expect(h.section.hidden).toBe(false);

  fakeWindow.fire("popstate");
  expect(h.section.hidden).toBe(true);
});
