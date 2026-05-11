import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { installConsoleCapture } from "./utils/consoleCapture";
import "./styles.css";
import "@xterm/xterm/css/xterm.css";

// E2E diagnostics: `vite build --mode development` keeps this; the
// production build's tree-shaker drops both the call and the import.
if (import.meta.env.DEV) {
  installConsoleCapture();
}

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("missing #root");

ReactDOM.createRoot(rootEl).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
