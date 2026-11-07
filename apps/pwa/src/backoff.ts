// The native client's reconnect numbers (`apps/native/src/connection.rs`,
// `BACKOFF_BASE`/`BACKOFF_CAP` and `backoff_delay`): 500 ms doubling per
// attempt, capped at 10 s, no jitter.
export const BACKOFF_BASE_MS = 500;
export const BACKOFF_CAP_MS = 10_000;

/** How long to wait before attempt `attempt`, counting from 0. */
export function backoffDelayMs(attempt: number): number {
  return Math.min(BACKOFF_BASE_MS * 2 ** attempt, BACKOFF_CAP_MS);
}
