/**
 * Capture `console.*` calls and global errors into `window.__rt_console`
 * so E2E tests can dump WebView diagnostics on failure without DevTools.
 *
 * Guarded by `import.meta.env.DEV`: included in `vite build --mode development`
 * (what the E2E harness runs) but tree-shaken out of the production
 * bundle. The captured array is bounded to keep memory predictable in
 * long-running sessions; oldest entries are dropped first.
 */

const MAX_ENTRIES = 500;

interface ConsoleEntry {
  level: string;
  args: unknown[];
  ts: number;
}

declare global {
  interface Window {
    __rt_console?: ConsoleEntry[];
  }
}

const LEVELS = ["log", "info", "warn", "error", "debug"] as const;

export function installConsoleCapture(): void {
  if (window.__rt_console) return;
  const buf: ConsoleEntry[] = [];
  window.__rt_console = buf;

  const push = (level: string, args: unknown[]): void => {
    buf.push({ level, args, ts: Date.now() });
    if (buf.length > MAX_ENTRIES) buf.splice(0, buf.length - MAX_ENTRIES);
  };

  for (const level of LEVELS) {
    const original = console[level].bind(console);
    console[level] = (...args: unknown[]): void => {
      try {
        push(level, args);
      } catch {
        // never let capture break console
      }
      original(...args);
    };
  }

  window.addEventListener("error", (e) => {
    push("window.error", [
      e.message,
      `${e.filename}:${e.lineno}:${e.colno}`,
      String(e.error),
    ]);
  });

  window.addEventListener("unhandledrejection", (e) => {
    push("unhandled-rejection", [String(e.reason)]);
  });
}
