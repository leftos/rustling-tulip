import { SW_SCOPE, SW_URL } from "./paths";

// Registers the hand-written service worker, which precaches the shell so an
// installed app opens offline. A failure is logged, never thrown: the page
// works without it.
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register(SW_URL, { scope: SW_SCOPE }).catch((error: unknown) => {
    console.error("service worker registration failed", error);
  });
}
