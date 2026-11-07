import { needsYouCount, type HomeModel } from "./home-model";

// The Badging API, which the DOM lib does not carry: the phone's Home Screen
// icon badge, set from the count of sessions that need the user.
declare global {
  interface Navigator {
    setAppBadge(contents?: number): Promise<void>;
    clearAppBadge(): Promise<void>;
  }
}

/** The Badging API behind one seam; a test hands in a spy. */
export interface BadgeEnv {
  set(count: number): Promise<void>;
  clear(): Promise<void>;
}

/** The page's own Badging API, or null when the browser has none. */
export function browserBadgeEnv(): BadgeEnv | null {
  if (!("setAppBadge" in navigator) || !("clearAppBadge" in navigator)) {
    return null;
  }
  return {
    set: (count) => navigator.setAppBadge(count),
    clear: () => navigator.clearAppBadge(),
  };
}

/**
 * Applies the Needs you count of each loaded Home model to the badge: the
 * first loaded model always applies, an unchanged count repeats nothing, and a
 * count of zero clears the badge. Best effort, as the service worker's push
 * badge is: a refused call is logged and its count is not recorded as applied,
 * so the next model that carries it applies it again.
 */
export function createBadge(env: BadgeEnv | null): { update(model: HomeModel): void } {
  /** The count the API last applied; null until the first one applies. */
  let applied: number | null = null;

  function fail(count: number, error: unknown): void {
    if (applied === count) {
      applied = null;
    }
    console.warn(count === 0 ? "clearAppBadge failed" : "setAppBadge failed", error);
  }

  return {
    update(model: HomeModel): void {
      // A reconnect resets the model to the empty one; the daemon's next
      // `sessions` message sets the badge again, so it must not clear it here.
      if (env === null || !model.loaded) {
        return;
      }
      const count = needsYouCount(model);
      if (count === applied) {
        return;
      }
      applied = count;
      const call = count === 0 ? env.clear() : env.set(count);
      void call.catch((error: unknown) => {
        fail(count, error);
      });
    },
  };
}
