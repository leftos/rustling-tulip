// The app is served from the daemon's origin root (`base: "/"` in
// `vite.config.ts`), so the page registers the worker at `/sw.js` with scope
// `/`. Only `main.ts` imports this module.
export const SW_URL = "/sw.js";
export const SW_SCOPE = "/";
