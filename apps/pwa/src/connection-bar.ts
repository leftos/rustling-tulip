import type { ConnectionStateName } from "./connection-machine";

/**
 * The connection bar Home and the session screen both draw, from one rule so
 * the two never disagree.
 */

/** The bar's texts. */
export const RECONNECTING_BAR = "Reconnecting…";
export const OFFLINE_BAR = "Offline";

/** The statuses a screen that has shown stays up through: the connection is coming back. */
const RECONNECTING: ReadonlySet<ConnectionStateName> = new Set([
  "connecting",
  "handshaking",
  "backoff",
  "hidden",
]);

/**
 * Whether the page has had a ready, paired connection in this visit. Home and
 * the session screen hold the one latch, so whichever is up at the first
 * `ready` counts it, and before it neither shows `Reconnecting…`.
 */
export interface ConnectedLatch {
  connected(): boolean;
  mark(): void;
}

/** A latch that starts unset and, once marked, stays set for the page visit. */
export function createConnectedLatch(): ConnectedLatch {
  let connected = false;
  return {
    connected: () => connected,
    mark: () => {
      connected = true;
    },
  };
}

/**
 * The bar at `status`: `Reconnecting…` once the screen has shown and the
 * connection is coming back, which wins over a refused send's `Offline`;
 * null when the connection is fine.
 */
export function connectionBar(
  status: ConnectionStateName,
  shown: boolean,
  sendRefused: boolean,
): string | null {
  if (shown && RECONNECTING.has(status)) {
    return RECONNECTING_BAR;
  }
  return sendRefused ? OFFLINE_BAR : null;
}
