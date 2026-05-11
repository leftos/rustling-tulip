/**
 * Helpers for setting up and tearing down a clean app+daemon environment
 * around an e2e run. Used by both wdio.conf.ts and the host.
 */
import { unlink } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";

import { handshakeFilePath, readHandshake } from "./handshake.js";
import { DaemonWsClient } from "./ws-client.js";

/**
 * Send `Shutdown` to any daemon that's already running, then wait for
 * `daemon.json` to disappear. No-ops if no daemon is up. Lets the next test
 * run start with a fresh daemon process — important when we're trying to
 * inject a different `RUSTLING_TULIP_CLAUDE` value into its environment.
 */
export async function shutdownExistingDaemon(opts: {
  timeoutMs?: number;
} = {}): Promise<{ killed: boolean }> {
  const timeout = opts.timeoutMs ?? 4_000;
  let client: DaemonWsClient | null = null;
  try {
    client = await DaemonWsClient.open();
  } catch {
    // No daemon, or daemon.json points to a stale port. Try to scrub the file.
    await unlink(handshakeFilePath()).catch(() => undefined);
    return { killed: false };
  }
  try {
    client.send({ type: "shutdown" });
  } catch {
    // socket may have closed mid-send; the daemon's still being asked to die
  }
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    try {
      await readHandshake();
      await delay(100);
    } catch {
      return { killed: true };
    }
  }
  // Give up waiting; remove the stale file so the next supervisor poll
  // doesn't latch onto it.
  await unlink(handshakeFilePath()).catch(() => undefined);
  return { killed: true };
}
