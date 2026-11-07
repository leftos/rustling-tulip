import { mountHomeView } from "./home-view";
import { reset as resetHome, type HomeModel } from "./home-model";
import { SW_SCOPE, SW_URL } from "./paths";
import { mountPairingView } from "./pairing-view";
import { browserScannerEnv, createCodeScanner } from "./scanner";
import { parseRoute, type Route } from "./session-screen";
import { mountSessionView } from "./session-view";
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
// camera; Home fills its own once the phone is paired and connected; every
// other notice stays on the console.
const pairingSection = document.getElementById("pairing");
if (pairingSection !== null) {
  mountPairingView(pairingSection, connection, createCodeScanner(browserScannerEnv()));
}
const intro = document.getElementById("intro");
const homeSection = document.getElementById("home");
// Home's one model: Home folds it, and a session's screen reads it to name
// its session and to tell when the session is gone.
let homeModel: HomeModel = resetHome();
if (homeSection !== null) {
  mountHomeView(homeSection, connection, intro, openSession, (model) => {
    homeModel = model;
  });
}

// The session screen owns the page at `/s/{id}`, and Home owns it everywhere
// else: the route is the page's own path, so a deep link and the browser's
// back button both land where the URL says.
const sessionSection = document.getElementById("session");
let mounted: { readonly id: string; readonly unmount: () => void } | null = null;
/** Whether this visit pushed the session's history entry, so Back pops it. */
let pushed = false;

function currentRoute(): Route {
  return parseRoute(location.pathname);
}

function show(): void {
  const route = currentRoute();
  document.body.dataset["route"] = route.kind;
  if (route.kind === "home" || sessionSection === null) {
    mounted?.unmount();
    mounted = null;
    return;
  }
  if (mounted !== null && mounted.id === route.id) {
    return;
  }
  mounted?.unmount();
  mounted = {
    id: route.id,
    unmount: mountSessionView(sessionSection, {
      sessionId: route.id,
      connection,
      home: () => homeModel,
      onBack: back,
    }),
  };
}

function openSession(sessionId: string): void {
  history.pushState(null, "", `/s/${encodeURIComponent(sessionId)}`);
  pushed = true;
  show();
}

function back(): void {
  if (pushed) {
    history.back();
    return;
  }
  // A page opened straight at a session has no Home entry behind it.
  history.replaceState(null, "", "/");
  show();
}

window.addEventListener("popstate", () => {
  pushed = currentRoute().kind === "session";
  show();
});
show();
connection.subscribe((notice) => {
  if (notice.kind === "pairing") {
    return;
  }
  console.info("pwa connection", notice);
});
connection.start();
