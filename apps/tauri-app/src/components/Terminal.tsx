import { useEffect, useRef } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import {
  base64ToBytes,
  bytesToBase64,
  loadScrollback,
  type DaemonClient,
} from "../api";

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
}

export default function Terminal({ sessionId, client, subscribePty, status }: Props) {
  const statusRef = useRef(status);
  statusRef.current = status;
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;

    const term = new XTerm({
      cursorBlink: true,
      fontFamily:
        "Cascadia Mono, Consolas, 'Courier New', monospace",
      fontSize: 13,
      theme: {
        background: "#0e1116",
        foreground: "#d6d6d6",
      },
      scrollback: 5000,
      convertEol: false,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(containerRef.current);

    termRef.current = term;
    fitRef.current = fit;

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
