import { expect, test } from "vitest";

import { FAKE_KEYS, memoryPushPort } from "./push-port";

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
