/**
 * Where the page's device token lives. The page uses
 * [`indexedDbTokenStore`], so a paired phone keeps its token across launches;
 * a phone with none presents no token, and the daemon answers `Welcome` and
 * then `DevicePairFailed`, which opens the pairing channel.
 */
export interface TokenStore {
  get(): Promise<string | undefined>;
  set(token: string): Promise<void>;
  clear(): Promise<void>;
}

/** A store held in memory only: nothing outlives the page. */
export function memoryTokenStore(initial?: string): TokenStore {
  let token = initial;
  return {
    get: () => Promise.resolve(token),
    set: (next) => {
      token = next;
      return Promise.resolve();
    },
    clear: () => {
      token = undefined;
      return Promise.resolve();
    },
  };
}

const DATABASE_NAME = "rustling-tulip";
const DATABASE_VERSION = 1;
const OBJECT_STORE = "device";
const TOKEN_KEY = "token";
const DEFAULT_PORT = 443;

/** The part of the page's `location` that names the host it pairs with. */
export type PageLocation = Pick<Location, "hostname" | "port">;

/**
 * The value under the `token` key: the token and the host and port it is
 * for, so the record names its host without relying on the page's origin.
 */
interface PairingRecord {
  readonly host: string;
  readonly port: number;
  readonly token: string;
}

/**
 * A store kept in IndexedDB (database `rustling-tulip`, object store
 * `device`, key `token`), which a service worker can read too. The value is a
 * {@link PairingRecord} for the host and port of `where` when `set` runs;
 * `get` answers its token only while `where` names the same host and port,
 * and leaves a record for another host in place. Each call opens the
 * database and runs one transaction; a failure rejects the call.
 */
export function indexedDbTokenStore(
  factory: IDBFactory = indexedDB,
  where: PageLocation = location,
): TokenStore {
  return {
    get: async () => {
      const value = await inTransaction(factory, "readonly", (store) => store.get(TOKEN_KEY));
      const here = hostOf(where);
      return isPairingRecord(value) && value.host === here.host && value.port === here.port
        ? value.token
        : undefined;
    },
    set: async (token) => {
      const record: PairingRecord = { ...hostOf(where), token };
      await inTransaction(factory, "readwrite", (store) => store.put(record, TOKEN_KEY));
    },
    clear: async () => {
      await inTransaction(factory, "readwrite", (store) => store.delete(TOKEN_KEY));
    },
  };
}

/** `location.port` is empty for the scheme's default port, 443 for `https`. */
function hostOf(where: PageLocation): { host: string; port: number } {
  return { host: where.hostname, port: where.port === "" ? DEFAULT_PORT : Number(where.port) };
}

function isPairingRecord(value: unknown): value is PairingRecord {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const record = value as Record<string, unknown>;
  return (
    typeof record["host"] === "string" &&
    typeof record["port"] === "number" &&
    typeof record["token"] === "string"
  );
}

function openDatabase(factory: IDBFactory): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = factory.open(DATABASE_NAME, DATABASE_VERSION);
    request.onupgradeneeded = () => {
      request.result.createObjectStore(OBJECT_STORE);
    };
    request.onsuccess = () => {
      resolve(request.result);
    };
    request.onerror = () => {
      reject(request.error ?? new Error("opening the token database failed"));
    };
    request.onblocked = () => {
      reject(new Error("opening the token database was blocked"));
    };
  });
}

async function inTransaction(
  factory: IDBFactory,
  mode: IDBTransactionMode,
  action: (store: IDBObjectStore) => IDBRequest,
): Promise<unknown> {
  const database = await openDatabase(factory);
  try {
    return await new Promise<unknown>((resolve, reject) => {
      const transaction = database.transaction(OBJECT_STORE, mode);
      const request = action(transaction.objectStore(OBJECT_STORE));
      transaction.oncomplete = () => {
        resolve(request.result);
      };
      transaction.onerror = () => {
        reject(transaction.error ?? new Error("the token transaction failed"));
      };
      transaction.onabort = () => {
        reject(transaction.error ?? new Error("the token transaction was aborted"));
      };
    });
  } finally {
    database.close();
  }
}
