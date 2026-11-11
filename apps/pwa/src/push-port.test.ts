import { expect, test } from "vitest";

import {
  browserPushPort,
  FAKE_KEYS,
  memoryPushPort,
  readyWithin,
  WORKER_NOT_READY,
  WORKER_READY_MS,
} from "./push-port";

test("a memory port starts where the test puts it", async () => {
  const port = memoryPushPort({ supported: false, installed: false, permission: "denied" });
  expect(port.supported()).toBe(false);
  expect(port.installed()).toBe(false);
  expect(port.permission()).toBe("denied");
  expect(await port.current()).toBeNull();
});

test("an undecided permission becomes granted on the ask", async () => {
  const port = memoryPushPort({ permission: "default" });
  expect(await port.requestPermission()).toBe("granted");
  expect(port.permission()).toBe("granted");
  expect(port.calls).toEqual(["requestPermission"]);
});

test("a denied permission stays denied", async () => {
  const port = memoryPushPort({ permission: "denied" });
  expect(await port.requestPermission()).toBe("denied");
});

test("readyWithin settles with the registration when it is ready first", async () => {
  const registration = { scope: "test" };
  const never = (): Promise<void> => new Promise<void>(() => {});
  await expect(readyWithin(Promise.resolve(registration), WORKER_READY_MS, never)).resolves.toBe(
    registration,
  );
});

test("readyWithin rejects with WORKER_NOT_READY when the worker never gets ready", async () => {
  const failure = await readyWithin(new Promise<never>(() => {}), 25, async () => {}).catch(
    (error: unknown) => error,
  );
  expect(failure).toBeInstanceOf(Error);
  expect((failure as Error).message).toBe(WORKER_NOT_READY);
});

test("a browser port whose worker never gets ready fails subscribe and current", async () => {
  const slept: number[] = [];
  const port = browserPushPort({
    ready: () => new Promise(() => {}),
    sleep: async (ms) => {
      slept.push(ms);
    },
  });
  await expect(port.subscribe("vapid-key")).rejects.toThrow(WORKER_NOT_READY);
  await expect(port.current()).rejects.toThrow(WORKER_NOT_READY);
  expect(slept).toEqual([WORKER_READY_MS, WORKER_READY_MS]);
});

test("subscribe holds the keys it makes and unsubscribe drops them", async () => {
  const port = memoryPushPort();
  expect(await port.subscribe("vapid-key")).toEqual(FAKE_KEYS);
  expect(port.held()).toEqual(FAKE_KEYS);
  expect(await port.current()).toEqual(FAKE_KEYS);
  expect(port.asked).toEqual(["vapid-key"]);
  await port.unsubscribe();
  expect(port.held()).toBeNull();
  expect(await port.current()).toBeNull();
  expect(port.calls).toEqual(["subscribe", "current", "unsubscribe", "current"]);
});
