import { useEffect, useState } from "react";
import {
  CLIPBOARD_COPY_EVENT,
  type ClipboardCopyDetail,
} from "../utils/clipboard";

interface PulseState {
  /// Monotonically increasing id. Used as the React `key` so a fresh
  /// copy mid-animation restarts the fade from frame 0 instead of
  /// dropping the second confirmation on the floor.
  id: number;
  source: string;
  length: number;
}

/// Tiny "copied" chip that fades in/out near the bottom of the sidebar
/// whenever `copyToClipboard` succeeds anywhere in the app. The
/// component is presentation-only: it subscribes to the
/// `rt:clipboard-copy` window event, schedules a self-clear, and renders
/// nothing the rest of the time. Pop-out windows mount their own React
/// tree (no Sidebar), so the pulse only appears in the main window —
/// which is the surface the user is looking at when triggering most
/// copies anyway.
export default function CopyPulse() {
  const [pulse, setPulse] = useState<PulseState | null>(null);

  useEffect(() => {
    let counter = 0;
    let clearTimer: number | null = null;
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<ClipboardCopyDetail>).detail;
      counter += 1;
      setPulse({
        id: counter,
        source: detail.source,
        length: detail.length,
      });
      if (clearTimer !== null) window.clearTimeout(clearTimer);
      // 1200ms matches the CSS keyframe duration — keeps React from
      // holding a stale node forever after the animation visually ends.
      clearTimer = window.setTimeout(() => {
        setPulse(null);
        clearTimer = null;
      }, 1200);
    };
    window.addEventListener(CLIPBOARD_COPY_EVENT, handler);
    return () => {
      window.removeEventListener(CLIPBOARD_COPY_EVENT, handler);
      if (clearTimer !== null) window.clearTimeout(clearTimer);
    };
  }, []);

  if (pulse === null) return null;

  return (
    <div
      className="copy-pulse-host"
      aria-live="polite"
      data-testid="copy-pulse"
    >
      <div
        className="copy-pulse"
        key={pulse.id}
        title={`copied ${pulse.length} chars`}
      >
        <span className="copy-pulse-check" aria-hidden="true">
          ✓
        </span>
        <span className="copy-pulse-label">copied</span>
      </div>
    </div>
  );
}
