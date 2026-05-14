import { useEffect, useRef } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { loadFonts, WebFontsAddon } from "@xterm/addon-web-fonts";
import { WebglAddon } from "@xterm/addon-webgl";
import {
  base64ToBytes,
  bytesToBase64,
  loadScrollback,
  type DaemonClient,
} from "../api";
import type { Agent, SessionMode } from "../types";
import { consumeAutoFocus } from "../utils/autofocus";
import { copyToClipboard } from "../utils/clipboard";
import { useFontSize } from "../utils/fontSize";
import { loadSettings } from "../utils/settings";
import type { EffectiveAppearance } from "../utils/appearance";
import { buildTerminalTheme } from "../utils/terminalTheme";

/// Historical cascade — preserved as the fallback when the user hasn't
/// picked a specific family in Settings. Geist Mono is the bundled
/// default; Cascadia / Consolas / Courier cover the half-second window
/// before Geist Mono finishes loading on a cold cache.
const DEFAULT_FONT_FAMILY =
  "'Geist Mono', 'Cascadia Mono', Consolas, 'Courier New', monospace";

/// Compose the family string handed to xterm. A user-picked family wins
/// the primary slot; the default cascade is appended so the renderer
/// has a fallback while the chosen font's webfont is still loading
/// (or, for system fonts, if the family resolves to nothing).
function buildFontFamily(picked: string | null): string {
  if (!picked) return DEFAULT_FONT_FAMILY;
  // Quote the picked family in case it contains spaces (most do).
  return `'${picked.replace(/'/g, "")}', ${DEFAULT_FONT_FAMILY}`;
}

// Module-scope singleton: kicks off the Geist Mono woff2 download on
// first import. WebGL bakes its glyph atlas at renderer-activate time,
// so the family must be in `document.fonts` before `term.open()` —
// otherwise the atlas is baked with the Cascadia fallback metrics and
// box-drawing characters land in the wrong columns (the failure mode
// that reverted commit e43688d's first WebGL attempt).
//
// Subsequent Terminal mounts await the already-resolved promise. The
// .catch makes loadFonts' rejection non-fatal: if the woff2 fails for
// any reason the WebFontsAddon instance below + WebGL will fall back to
// the next family in the `fontFamily` cascade (Cascadia / Consolas).
const geistMonoReady: Promise<unknown> = loadFonts(["Geist Mono"]).catch(
  (err: unknown) => {
    console.warn("Geist Mono load failed; xterm will use fallback", err);
  },
);

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
  /// Which agent is driving the session — selects the shift+enter
  /// escape sequence (claude wants `\\\r`, codex wants `\n`).
  agent: Agent;
  /// Session mode — `plain_shell` falls back to the claude sequence
  /// regardless of `agent` because `\\\r` is bash line-continuation;
  /// `\n` would just submit the partial command.
  mode: SessionMode;
  /// Effective app/repo/workspace/session appearance resolved by the
  /// owning pane. Threaded in so live inheritance updates can repaint
  /// xterm without reconnecting the PTY.
  appearance: EffectiveAppearance;
}

export default function Terminal({
  sessionId,
  client,
  subscribePty,
  status,
  tabId,
  agent,
  mode,
  appearance,
}: Props) {
  const statusRef = useRef(status);
  statusRef.current = status;
  // Shift+enter and Ctrl+C handlers read these from refs so changes
  // (e.g. a session that flipped agent — currently impossible, but
  // mode could change in the future) don't require re-wiring xterm.
  const agentRef = useRef(agent);
  agentRef.current = agent;
  const modeRef = useRef(mode);
  modeRef.current = mode;
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);

  const fontSize = useFontSize(tabId ?? null, appearance.terminal_font_size);
  const fontFamily = appearance.terminal_font_family;
  const fontBold = appearance.terminal_font_bold;
  const backgroundColor = appearance.terminal_background_color;

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

  /// Apply font-family / font-weight changes the same way. WebGL bakes
  /// the glyph atlas against the current family, so we also nudge the
  /// addon to relayout once the new face is in `document.fonts`. xterm
  /// re-measures its char dimensions when fontFamily / fontWeight
  /// change, so a `fit()` follow-up is required to keep cols/rows in
  /// sync with the new metrics.
  useEffect(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    // Preload the picked family so the atlas measurement that follows
    // sees the right metrics. Failure is non-fatal — the fallback
    // cascade still renders correctly.
    if (fontFamily) {
      void loadFonts([fontFamily]).catch((err: unknown) => {
        console.warn("font preload failed for", fontFamily, err);
      });
    }
    term.options.fontFamily = buildFontFamily(fontFamily);
    term.options.fontWeight = fontBold ? "bold" : "normal";
    fit.fit();
    client.send({
      type: "resize",
      session_id: sessionId,
      cols: term.cols,
      rows: term.rows,
    });
  }, [fontFamily, fontBold, client, sessionId]);

  /// Apply terminal palette changes independently from font changes. The
  /// xterm options setter needs a fresh object reference for structured
  /// options, so build a new theme each time the resolved background shifts.
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.theme = buildTerminalTheme(backgroundColor);
  }, [backgroundColor]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    let cancelled = false;
    const cleanupFns: Array<() => void> = [];

    // The whole setup is wrapped in an async IIFE so we can await the
    // Geist Mono font before `term.open()`. The cleanup return below
    // handles both the pre-resolve case (sets `cancelled` so the IIFE
    // bails out) and the post-resolve case (runs every registered
    // cleanup fn). Order of cleanup fns is preserved from the
    // pre-refactor synchronous shape — they run in insertion order,
    // not reverse, because the original cleanup wasn't strict LIFO.
    void (async () => {
      await geistMonoReady;
      if (cancelled) return;

      const term = new XTerm({
        cursorBlink: true,
        // Geist Mono first (vendored as /public/fonts/GeistMono-Variable.woff2,
        // pulled in via @font-face in styles.css). Cascadia/Consolas fall back
        // when the variable woff2 hasn't finished loading yet so the very
        // first paint isn't blank. The module-level `geistMonoReady` above
        // guarantees the woff2 is in `document.fonts` before this point so
        // WebGL's glyph-atlas measurement uses the correct metrics. A
        // user-picked family (Settings → Terminal → Font family) is
        // prepended to this cascade by buildFontFamily.
        fontFamily: buildFontFamily(fontFamily),
        fontWeight: fontBold ? "bold" : "normal",
        fontSize,
        theme: buildTerminalTheme(backgroundColor),
        scrollback: 5000,
        convertEol: false,
      });
      const fit = new FitAddon();
      term.loadAddon(fit);

      // WebFontsAddon must be loaded BEFORE `term.open()` per its README —
      // otherwise the renderer measures glyph widths against the fallback
      // font. The geistMonoReady await above already guarantees the font
      // is in document.fonts, so this addon is a belt-and-braces fix: if
      // anything goes wrong with the await path it will still call
      // relayout() once the font finishes loading.
      const webFonts = new WebFontsAddon();
      term.loadAddon(webFonts);

      term.open(container);

      // WebGL renderer — paints the whole frame in a single rAF, so even
      // non-DEC-2026 redraws (legacy codex, edge-case repaints) don't
      // expose intermediate cursor positions. Must be loaded AFTER
      // term.open() per xterm 6's deferred-activate model (the addon
      // self-registers via `core.onWillOpen()` if open hasn't fired yet,
      // but the explicit ordering avoids relying on that internal). The
      // sync try/catch covers `new WebglAddon()` (a Safari <16 WebGL2
      // probe lives there) and any sync throw inside loadAddon —
      // graceful fall-through leaves us on the DOM renderer.
      try {
        const webgl = new WebglAddon();
        webgl.onContextLoss(() => {
          webgl.dispose();
        });
        term.loadAddon(webgl);
        cleanupFns.push(() => {
          try {
            webgl.dispose();
          } catch {
            // already disposed via context loss or term.dispose cascade
          }
        });
      } catch (err) {
        console.warn(
          "WebGL renderer unavailable, falling back to DOM",
          err,
        );
      }

      termRef.current = term;
      fitRef.current = fit;

      // OSC 52 observer — fires the clipboard-copy event whenever a TUI
      // running in this terminal pushes text to the system clipboard via
      // the standard ESC]52;<Pc>;<Pd>BEL sequence. Claude Code, codex,
      // tmux, vim and friends use this to implement copy-on-select while
      // mouse-tracking is enabled (which prevents xterm's local
      // selection from firing onSelectionChange). We're a passive
      // observer: return false so xterm's native handling (when
      // allowProposedApi is on) is unaffected, and we never write to the
      // clipboard ourselves — the OSC sequence already did that.
      //
      // Format: <Pc>;<Pd> where Pc is the selection target ("c" for
      // clipboard, "p"/"s" for primary/select, etc.) and Pd is base64
      // payload OR "?" for a query. We ignore queries.
      const oscDisposable = term.parser.registerOscHandler(52, (data) => {
        const sep = data.indexOf(";");
        if (sep === -1) return false;
        const payload = data.substring(sep + 1);
        if (payload === "?" || payload.length === 0) return false;
        let length = 0;
        try {
          length = atob(payload).length;
        } catch {
          // Malformed base64 — still surface the pulse but with length 0
          // so the user gets feedback that *something* tried to write.
        }
        window.dispatchEvent(
          new CustomEvent("rt:clipboard-copy", {
            detail: { source: "OSC52", length },
          }),
        );
        return false;
      });
      cleanupFns.push(() => oscDisposable.dispose());

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
      let scrollbackCancelled = false;

      // Replay persisted scrollback first so users see their history before
      // any live PTY chunks arrive. The attach + subscribe happen after the
      // replay completes (or times out) to keep ordering correct.
      void (async () => {
        const sb = await loadScrollback(client, sessionId);
        if (scrollbackCancelled) return;
        if (sb && sb.data_b64.length > 0) {
          if (sb.truncated) {
            term.writeln("\x1b[33m[earlier output discarded]\x1b[0m");
          }
          term.write(
            decoder.decode(base64ToBytes(sb.data_b64), { stream: true }),
          );
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

      // Auto-copy any non-empty selection to the system clipboard. Fires
      // on every selection-change tick (mouse drag, shift+arrow, etc.) —
      // `navigator.clipboard.writeText` is async and cheap, and even if
      // the user is still dragging the last write wins. The .catch is a
      // safety net for transient permission failures; we don't surface
      // them because the user can always retry the selection.
      //
      // Gated behind `terminal.copy_on_selection` (opt-out, defaults
      // on). The ref tracks the latest value so toggling the setting
      // takes effect immediately without re-wiring xterm. Ctrl+C and
      // Ctrl+Shift+C remain unconditional copy bindings.
      let autoCopyEnabled = loadSettings().terminal.copy_on_selection;
      const settingsListener = () => {
        autoCopyEnabled = loadSettings().terminal.copy_on_selection;
      };
      window.addEventListener("rt:settings-changed", settingsListener);
      const selectionHandler = term.onSelectionChange(() => {
        if (!autoCopyEnabled) return;
        const sel = term.getSelection();
        if (sel.length === 0) return;
        void copyToClipboard(sel, "selection").catch(() => {});
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
      //   * **Ctrl/Cmd+C** — copy the current selection iff one exists,
      //     otherwise fall through so xterm's default sends ^C (SIGINT)
      //     to the PTY. Matches Windows Terminal / VS Code terminal
      //     conventions. Ctrl+Shift+C remains as an always-copy variant
      //     for users who want to copy without first selecting.
      //   * **Shift+Enter** — insert a newline into the agent's input
      //     buffer without submitting. The required sequence is agent-
      //     specific: Claude treats `\` + CR as line-continuation (matches
      //     the `\\\r` sequence Claude Code's `terminal-setup` injects
      //     into VS Code's keybindings), while codex's TUI binds `Ctrl+J`
      //     (`\n`) to `insert_newline`. Plain-shell sessions stay on
      //     `\\\r` so bash line-continuation keeps working.
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
          // intended sequence; Claude then sees CR + LF and submits
          // anyway). Returning false alone isn't enough — xterm skips
          // its own processing but doesn't preventDefault for us.
          event.preventDefault();
          if (stopped) return false;
          const seq =
            modeRef.current === "plain_shell" || agentRef.current === "claude"
              ? "\\\r"
              : "\n";
          const enc = new TextEncoder();
          client.send({
            type: "send_input",
            session_id: sessionId,
            data_b64: bytesToBase64(enc.encode(seq)),
          });
          return false;
        }

        if (!mod) return true;
        const key = event.key.toLowerCase();

        // Step aside for paste — see the block comment above.
        if (key === "v") return false;

        // Ctrl+Shift+C: always copy the current selection if one exists.
        // Ctrl+C: copy if there's a selection, otherwise fall through so
        // xterm sends ^C (SIGINT) to the PTY.
        //
        // `event.preventDefault()` is required alongside `return false`
        // when we're suppressing the ^C — without it, the keydown still
        // reaches xterm's helper-textarea path and the PTY sees SIGINT
        // anyway. Codex in particular treats the first SIGINT as
        // "clear input buffer" and the second as "exit", so a stray
        // ^C while copying a highlighted region would wipe whatever
        // the user was composing. Same shape as the Shift+Enter branch
        // above.
        if (key === "c") {
          const sel = term.getSelection();
          if (sel.length === 0) {
            // Shift+Ctrl+C with no selection is a no-op (don't send ^C
            // — the shift modifier signals intent to copy, not interrupt).
            if (event.shiftKey) {
              event.preventDefault();
              return false;
            }
            return true;
          }
          event.preventDefault();
          void copyToClipboard(sel, "Ctrl+C").catch(() => {});
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
      resizeObserver.observe(container);

      // Disposers run in insertion order on unmount. Order matches the
      // pre-refactor cleanup (onData → unsubPty → resizeObs → detach →
      // __rt_terms delete → webFonts → term.dispose) so the WS detach
      // fires before the term is disposed.
      cleanupFns.push(() => {
        scrollbackCancelled = true;
      });
      cleanupFns.push(() => onDataHandler.dispose());
      cleanupFns.push(() => selectionHandler.dispose());
      cleanupFns.push(() =>
        window.removeEventListener("rt:settings-changed", settingsListener),
      );
      cleanupFns.push(() => unsubPty());
      cleanupFns.push(() => resizeObserver.disconnect());
      cleanupFns.push(() =>
        client.send({ type: "detach", session_id: sessionId }),
      );
      cleanupFns.push(() => {
        const w = window as unknown as { __rt_terms?: Map<string, XTerm> };
        w.__rt_terms?.delete(sessionId);
      });
      cleanupFns.push(() => {
        try {
          webFonts.dispose();
        } catch {
          // already disposed via term.dispose cascade
        }
      });
      cleanupFns.push(() => term.dispose());
      cleanupFns.push(() => {
        termRef.current = null;
        fitRef.current = null;
      });
    })();

    return () => {
      cancelled = true;
      for (const fn of cleanupFns) {
        try {
          fn();
        } catch (err) {
          console.warn("terminal cleanup step threw", err);
        }
      }
    };
  }, [sessionId, client, subscribePty]);

  return (
    <div
      ref={containerRef}
      className="terminal-container"
      style={{ backgroundColor }}
      data-testid="terminal-container"
      data-session-id={sessionId}
    />
  );
}
