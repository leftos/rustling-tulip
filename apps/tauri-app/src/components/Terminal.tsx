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
}

export default function Terminal({ sessionId, client, subscribePty }: Props) {
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

    // Dev-only: expose XTerm instances so the e2e harness can read the
    // scrollback buffer programmatically (xterm renders to a canvas, so DOM
    // assertions can't see the text). Bundlers tree-shake `import.meta.env.DEV`
    // false branches in production builds, so this never ships to users.
    if (import.meta.env.DEV) {
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
      if (import.meta.env.DEV) {
        const w = window as unknown as { __rt_terms?: Map<string, XTerm> };
        w.__rt_terms?.delete(sessionId);
      }
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
