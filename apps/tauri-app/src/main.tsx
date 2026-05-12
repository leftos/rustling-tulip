import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { installConsoleCapture } from "./utils/consoleCapture";
// Side-effecting Monaco worker bootstrap; must run before any DiffEditor
// mounts. See `utils/monacoSetup.ts` for the rationale on which workers
// are bundled.
import "./utils/monacoSetup";
import "./styles.css";
import "@xterm/xterm/css/xterm.css";

// E2E diagnostics: `vite build --mode development` keeps this; the
// production build's tree-shaker drops both the call and the import.
if (import.meta.env.DEV) {
  installConsoleCapture();
}

// Suppress the default WebView context menu globally. The app surfaces
// its own context menus where they're useful (session leaves, pane
// headers, tab bar, source-control entries); everywhere else right-click
// is a no-op. Components that want a custom menu call `e.preventDefault()`
// in their own onContextMenu handler and render their menu — this
// listener runs in the bubble phase, so it doesn't fire when a child has
// already handled the event.
//
// One opt-out: contenteditable / Monaco editor surfaces declare
// `data-allow-context-menu` on themselves or any ancestor so users keep
// the OS clipboard menu (Copy/Cut/Paste/Inspect Element) where the WebView
// would normally provide it.
document.addEventListener("contextmenu", (e) => {
  const target = e.target;
  if (target instanceof Element && target.closest("[data-allow-context-menu]")) {
    return;
  }
  e.preventDefault();
});

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("missing #root");

ReactDOM.createRoot(rootEl).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
