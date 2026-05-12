import { useEffect, useRef } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { CanvasAddon } from "@xterm/addon-canvas";
import {
  base64ToBytes,
  bytesToBase64,
  loadScrollback,
  type DaemonClient,
} from "../api";
import { consumeAutoFocus } from "../utils/autofocus";
import { useFontSize } from "../utils/fontSize";

interface Props {
  sessionId: string;
  client: DaemonClient;
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
  /// Latest known status. Threaded through so the input handler can
  /// skip `send_input` after the daemon has already reported the
  /// session as stopped/errored — between the Stopped broadcast
  /// arriving and React unmounting this component there's a small
  /// window where keystrokes would otherwise be forwarded to a dying
  /// PTY (and trigger daemon warnings).
  status: string;
  /// Tab id this terminal is mounted under. Resolves the per-tab font
  /// size override. `null` in pop-out windows.
  tabId?: string | null;
}

export default function Terminal({
  sessionId,
  client,
  subscribePty,
  status,
  tabId,
}: Props) {
  const statusRef = useRef(status);
  statusRef.current = status;
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);

  const fontSize = useFontSize(sessionId, tabId ?? null);

  /// Apply font-size changes to the live XTerm without remounting it —
  /// rebuilding the terminal on every size change would clear scrollback
  /// and detach/reattach the PTY. xterm.js exposes `options.fontSize` as
  /// a setter; we follow up with `fit()` and a daemon resize so the PTY
  /// dims match the new render.
  useEffect(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    term.options.fontSize = fontSize;
    fit.fit();
    client.send({
      type: "resize",
      session_id: sessionId,
      cols: term.cols,
      rows: term.rows,
    });
  }, [fontSize, client, sessionId]);

  useEffect(() => {
    if (!containerRef.current) return;

    const term = new XTerm({
      cursorBlink: true,
      // Geist Mono first (vendored as /public/fonts/GeistMono-Variable.woff2,
      // pulled in via @font-face in styles.css). Cascadia/Consolas fall back
      // when the variable woff2 hasn't finished loading yet so the very
      // first paint isn't blank.
      fontFamily:
        "'Geist Mono', 'Cascadia Mono', Consolas, 'Courier New', monospace",
      fontSize,
      theme: {
        // Mirrors the design-token palette in styles.css :root. Keeping
        // these in sync means the terminal feels flush with the pane
        // chrome instead of floating on a slightly different shade. If
        // the tokens change, change these too.
        background: "#08090b",
        foreground: "#e5e6e8",
        cursor: "#5b9bff",
        cursorAccent: "#08090b",
        selectionBackground: "rgba(91, 155, 255, 0.30)",
        selectionForeground: "#f5f6f8",
        // ANSI 16-color palette — tuned for the cool-neutral near-black
        // background. Normal slots stay calm; bright slots add lightness
        // for visible contrast without the saturation jump that makes
        // default xterm palettes feel '90s.
        black: "#16181d",
        red: "#ef5c5c",
        green: "#3fb96a",
        yellow: "#e8a531",
        blue: "#5b9bff",
        magenta: "#b787f0",
        cyan: "#5dd5e3",
        white: "#e5e6e8",
        brightBlack: "#656872",
        brightRed: "#ff8585",
        brightGreen: "#62d18a",
        brightYellow: "#f5c267",
        brightBlue: "#7eb4ff",
        brightMagenta: "#d4abff",
        brightCyan: "#8feaf3",
        brightWhite: "#f5f6f8",
      },
      scrollback: 5000,
      convertEol: false,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(containerRef.current);

    // Canvas renderer drastically reduces flicker on full-page TUI
    // redraws (codex/htop/vim style). The default DOM renderer
    // updates per-cell DOM nodes -- claude's mostly-line-append
    // output runs fine on it, but codex's full-buffer repaint every
    // frame thrashes layout and the cursor visibly jumps around.
    // Canvas paints the whole grid in one pass per frame so updates
    // stay coherent.
    //
    // Why Canvas and not WebGL: @xterm/addon-webgl @ 0.19 still has
    // a teardown bug where, if WebGL activation fails *after*
    // `loadAddon` registers it (any GPU/context hiccup), the
    // addon's `dispose()` crashes with `Cannot read properties of
    // undefined (reading '_isDisposed')` on Terminal unmount.
    // CanvasAddon has no equivalent issue and is plenty fast for
    // full-buffer TUI redraws. Revisit on xterm 6.x.
    try {
      const canvas = new CanvasAddon();
      term.loadAddon(canvas);
    } catch (err) {
      console.warn("xterm Canvas renderer unavailable, falling back to DOM", err);
    }

    termRef.current = term;
    fitRef.current = fit;

    // Spawn flow and pop-out windows mark the session id so the xterm
    // helper textarea grabs OS keyboard focus on mount — without this the
    // user has to click into the terminal before they can type. Deferred
    // by one frame so the focus call happens after React commits and the
    // browser has painted the container; calling synchronously here works
    // too, but the rAF tick avoids a flash where focus briefly lands on
    // an element about to be re-laid-out by `fit.fit()`.
    if (consumeAutoFocus(sessionId)) {
      requestAnimationFrame(() => {
        if (termRef.current === term) term.focus();
      });
    }

    // Expose XTerm instances on `window.__rt_terms` so the e2e harness (and
    // anyone poking around in devtools) can read the scrollback buffer.
    // xterm renders to a canvas, so DOM assertions can't see the text. The
    // map holds only references to terminals already alive in React state,
    // and the cleanup branch below removes them on unmount — no leak risk.
    {
      const w = window as unknown as { __rt_terms?: Map<string, XTerm> };
      const map = w.__rt_terms ?? new Map<string, XTerm>();
      map.set(sessionId, term);
      w.__rt_terms = map;
    }

    const decoder = new TextDecoder();
    let cancelled = false;

    // Replay persisted scrollback first so users see their history before
    // any live PTY chunks arrive. The attach + subscribe happen after the
    // replay completes (or times out) to keep ordering correct.
    void (async () => {
      const sb = await loadScrollback(client, sessionId);
      if (cancelled) return;
      if (sb && sb.data_b64.length > 0) {
        if (sb.truncated) {
          term.writeln("[33m[earlier output discarded][0m");
        }
        term.write(decoder.decode(base64ToBytes(sb.data_b64), { stream: true }));
      }
      requestAnimationFrame(() => {
        fit.fit();
        client.send({
          type: "resize",
          session_id: sessionId,
          cols: term.cols,
          rows: term.rows,
        });
        client.send({ type: "attach", session_id: sessionId });
      });
    })();

    const unsubPty = subscribePty(sessionId, (b64) => {
      const bytes = base64ToBytes(b64);
      term.write(decoder.decode(bytes, { stream: true }));
    });

    const onDataHandler = term.onData((data) => {
      // Skip when the latest known status is terminal — there's a small
      // race window between the daemon's Stopped/Error broadcast and
      // React unmounting this component, and we don't want to send
      // keystrokes to a dying PTY.
      if (statusRef.current === "stopped" || statusRef.current === "error") {
        return;
      }
      const enc = new TextEncoder();
      client.send({
        type: "send_input",
        session_id: sessionId,
        data_b64: bytesToBase64(enc.encode(data)),
      });
    });

    // Custom keymap on top of xterm's default. Returning `false` from
    // this handler tells xterm to bail out *before* it processes the
    // keystroke — meaning xterm will not generate any PTY bytes AND will
    // not call `event.preventDefault()`. The latter half matters for
    // paste: if xterm calls preventDefault on a Ctrl+V keydown, the
    // browser silently skips firing its synthetic `paste` event, and the
    // native xterm `paste` DOM-event listener on the helper textarea
    // never gets a chance to run.
    //
    //   * **Ctrl/Cmd+V** and **Ctrl/Cmd+Shift+V** — return false so the
    //     browser fires its native paste event; xterm's textarea paste
    //     listener reads `clipboardData`, pipes through inputHandler.paste
    //     (with bracketed-paste mode), and fires onData. Without this
    //     case, xterm's default keydown sends `\x16` (SYN) to the PTY
    //     and preventDefaults the event, suppressing paste entirely.
    //     Iter 50 added a manual paste path that double-fired alongside
    //     the native handler; iter 51 dropped it but accidentally also
    //     broke the native path because xterm's default still ate Ctrl+V;
    //     iter 52 fixes that by explicitly returning false.
    //   * **Ctrl/Cmd+Shift+C** — copy the current selection. xterm has
    //     no default binding for this; read via `term.getSelection()`
    //     and write through `navigator.clipboard.writeText()`. Write-
    //     clipboard does not trigger a permission prompt for user-
    //     initiated actions.
    //   * **Shift+Enter** — send `\` + CR. Claude and codex TUIs convert
    //     this to "newline within input without submitting"; bash treats
    //     it as a line continuation so plain-shell sessions don't break.
    //     Matches the `\\\r` sequence Claude Code's `terminal-setup`
    //     injects into VS Code's terminal keybindings.
    //
    // Bare Ctrl+C still reaches the PTY as ^C (SIGINT) — only the
    // Ctrl+Shift+C combo is intercepted.
    term.attachCustomKeyEventHandler((event) => {
      if (event.type !== "keydown") return true;
      const mod = event.ctrlKey || event.metaKey;
      const stopped =
        statusRef.current === "stopped" || statusRef.current === "error";

      if (event.shiftKey && !mod && event.key === "Enter") {
        // We handle this combo ourselves — block the browser's default
        // so the helper textarea doesn't *also* receive an Enter
        // (which would insert "\n" into the textarea, trigger xterm's
        // input listener, and forward a stray "\n" to the PTY after our
        // intended "\\\r" sequence; Claude then sees CR + LF and
        // submits anyway). Returning false alone isn't enough — xterm
        // skips its own processing but doesn't preventDefault for us.
        event.preventDefault();
        if (stopped) return false;
        const enc = new TextEncoder();
        client.send({
          type: "send_input",
          session_id: sessionId,
          data_b64: bytesToBase64(enc.encode("\\\r")),
        });
        return false;
      }

      if (!mod) return true;
      const key = event.key.toLowerCase();

      // Step aside for paste — see the block comment above.
      if (key === "v") return false;

      if (event.shiftKey && key === "c") {
        const sel = term.getSelection();
        if (sel.length === 0) return true;
        navigator.clipboard.writeText(sel).catch(() => {});
        return false;
      }

      return true;
    });

    const resizeObserver = new ResizeObserver(() => {
      if (!fitRef.current || !termRef.current) return;
      fitRef.current.fit();
      client.send({
        type: "resize",
        session_id: sessionId,
        cols: termRef.current.cols,
        rows: termRef.current.rows,
      });
    });
    if (containerRef.current) resizeObserver.observe(containerRef.current);

    return () => {
      cancelled = true;
      onDataHandler.dispose();
      unsubPty();
      resizeObserver.disconnect();
      client.send({ type: "detach", session_id: sessionId });
      const w = window as unknown as { __rt_terms?: Map<string, XTerm> };
      w.__rt_terms?.delete(sessionId);
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [sessionId, client, subscribePty]);

  return (
    <div
      ref={containerRef}
      className="terminal-container"
      data-testid="terminal-container"
      data-session-id={sessionId}
    />
  );
}
