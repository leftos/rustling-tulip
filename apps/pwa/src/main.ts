import { SW_SCOPE, SW_URL } from "./paths";
import { mountPairingView } from "./pairing-view";
import { browserScannerEnv, createCodeScanner } from "./scanner";
import { createConnection, type WebSocketLike } from "./socket";
import { indexedDbTokenStore } from "./token-store";

// Registers the hand-written service worker, which precaches the shell so an
// installed app opens offline. A failure is logged, never thrown: the page
// works without it.
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register(SW_URL, { scope: SW_SCOPE }).catch((error: unknown) => {
    console.error("service worker registration failed", error);
  });
}

/** The real `WebSocket`, behind the connection's small surface. */
function openWebSocket(url: string): WebSocketLike {
  const socket = new WebSocket(url);
  const like: WebSocketLike = {
    send: (data) => {
      socket.send(data);
    },
    close: () => {
      socket.close();
    },
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
  };
  socket.onopen = (event) => {
    like.onopen?.(event);
  };
  socket.onmessage = (event) => {
    like.onmessage?.(event);
  };
  socket.onclose = (event) => {
    like.onclose?.(event);
  };
  socket.onerror = (event) => {
    like.onerror?.(event);
  };
  return like;
}

// The daemon's tailnet listener admits only its own origin, so the page dials
// the host it was served from. The device token is kept in IndexedDB; a
// phone with none lands on the pairing channel, driven through `askToPair`
// and `redeem`.
const connection = createConnection({
  url: `wss://${location.host}/ws`,
  openSocket: openWebSocket,
  setTimeout: (callback, delayMs) => window.setTimeout(callback, delayMs),
  clearTimeout: (handle) => {
    window.clearTimeout(handle);
  },
  visibility: {
    isHidden: () => document.visibilityState === "hidden",
    onChange: (callback) => {
      document.addEventListener("visibilitychange", callback);
      return () => {
        document.removeEventListener("visibilitychange", callback);
      };
    },
  },
  tokenStore: indexedDbTokenStore(),
});

// The pairing screen fills the page's section from the pairing model and the
// camera; every other notice stays on the console.
const pairingSection = document.getElementById("pairing");
if (pairingSection !== null) {
  mountPairingView(pairingSection, connection, createCodeScanner(browserScannerEnv()));
}
connection.subscribe((notice) => {
  if (notice.kind === "pairing") {
    return;
  }
  console.info("pwa connection", notice);
});
connection.start();
