import { IDBFactory } from "fake-indexeddb";
import { expect, test } from "vitest";

import { indexedDbTokenStore, memoryTokenStore, type PageLocation } from "./token-store";

const DEVICE_TOKEN = "device-token";
const HOST: PageLocation = { hostname: "host.tail1234.ts.net", port: "8787" };
const OTHER_HOST: PageLocation = { hostname: "other.tail1234.ts.net", port: "8787" };

// Each test gets a factory of its own, so no database leaks between them.
function freshFactory(): globalThis.IDBFactory {
  return new IDBFactory() as unknown as globalThis.IDBFactory;
}

/** The raw value under the `device` store's `token` key. */
function storedRecord(factory: globalThis.IDBFactory): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const open = factory.open("rustling-tulip");
    open.onerror = () => {
      reject(open.error ?? new Error("open failed"));
    };
    open.onsuccess = () => {
      const database = open.result;
      const request = database.transaction("device", "readonly").objectStore("device").get("token");
      request.onsuccess = () => {
        database.close();
        resolve(request.result);
      };
      request.onerror = () => {
        database.close();
        reject(request.error ?? new Error("get failed"));
      };
    };
  });
}

test("indexed_db_store_is_empty_before_any_set", async () => {
  const store = indexedDbTokenStore(freshFactory(), HOST);

  expect(await store.get()).toBeUndefined();
});

test("indexed_db_store_returns_the_token_it_was_given_on_the_same_host", async () => {
  const store = indexedDbTokenStore(freshFactory(), HOST);

  await store.set(DEVICE_TOKEN);

  expect(await store.get()).toBe(DEVICE_TOKEN);
});

test("indexed_db_store_keeps_a_pairing_record_of_host_port_and_token", async () => {
  const factory = freshFactory();

  await indexedDbTokenStore(factory, HOST).set(DEVICE_TOKEN);
  await indexedDbTokenStore(factory, { hostname: "default.tail1234.ts.net", port: "" }).set(
    "default-port-token",
  );

  expect(await storedRecord(factory)).toEqual({
    host: "default.tail1234.ts.net",
    port: 443,
    token: "default-port-token",
  });
});

test("indexed_db_store_reads_a_record_for_another_host_as_undefined_and_keeps_it", async () => {
  const factory = freshFactory();
  await indexedDbTokenStore(factory, OTHER_HOST).set(DEVICE_TOKEN);

  expect(await indexedDbTokenStore(factory, HOST).get()).toBeUndefined();
  expect(await indexedDbTokenStore(factory, { ...OTHER_HOST, port: "443" }).get()).toBeUndefined();
  expect(await storedRecord(factory)).toEqual({
    host: OTHER_HOST.hostname,
    port: 8787,
    token: DEVICE_TOKEN,
  });
});

test("indexed_db_store_keeps_the_token_for_a_later_launch", async () => {
  const factory = freshFactory();
  await indexedDbTokenStore(factory, HOST).set(DEVICE_TOKEN);

  const relaunched = indexedDbTokenStore(factory, HOST);

  expect(await relaunched.get()).toBe(DEVICE_TOKEN);
});

test("indexed_db_store_clear_forgets_the_token", async () => {
  const factory = freshFactory();
  const store = indexedDbTokenStore(factory, HOST);
  await store.set(DEVICE_TOKEN);

  await store.clear();

  expect(await store.get()).toBeUndefined();
  expect(await indexedDbTokenStore(factory, HOST).get()).toBeUndefined();
});

test("memory_store_sets_gets_and_clears", async () => {
  const store = memoryTokenStore();
  expect(await store.get()).toBeUndefined();

  await store.set(DEVICE_TOKEN);
  expect(await store.get()).toBe(DEVICE_TOKEN);

  await store.clear();
  expect(await store.get()).toBeUndefined();
});
