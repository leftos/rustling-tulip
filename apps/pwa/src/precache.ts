/// The quoted sentinel `sw.ts` holds where the build's asset list goes. The
/// `rt-precache` plugin in `vite.config.ts` replaces it with the list's JSON.
export const PRECACHE_SENTINEL = "__RT_PRECACHE__";

/// The build's hashed JS and CSS as the worker fetches them: every `assets/`
/// file that is not a source map, sorted and root-absolute. `sw.js` and
/// everything outside `assets/` are left to the fetch strategies.
export function precacheUrls(fileNames: readonly string[]): string[] {
  return fileNames
    .filter((fileName) => fileName.startsWith("assets/") && !fileName.endsWith(".map"))
    .sort()
    .map((fileName) => `/${fileName}`);
}

/// Replace the sentinel in the emitted worker with `urls` as a JS string
/// literal, so the worker parses the list with `JSON.parse`. The sentinel must
/// occur exactly once across its three quoted forms (`"`, `'` and `` ` ``, which
/// is how the bundler may reprint the literal): none means the worker has no
/// place for the list, and more than one would leave a stray occurrence.
export function injectPrecache(code: string, urls: readonly string[]): string {
  const sentinels = [
    `"${PRECACHE_SENTINEL}"`,
    `'${PRECACHE_SENTINEL}'`,
    `\`${PRECACHE_SENTINEL}\``,
  ];
  const occurrences = sentinels.reduce(
    (count, sentinel) => count + code.split(sentinel).length - 1,
    0,
  );
  if (occurrences !== 1) {
    throw new Error(
      `the service worker holds the precache sentinel ${occurrences} times, not once`,
    );
  }
  const literal = JSON.stringify(JSON.stringify(urls));
  return sentinels.reduce((replaced, sentinel) => replaced.replace(sentinel, () => literal), code);
}
