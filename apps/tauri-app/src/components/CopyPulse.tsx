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

/// How long the boot intro stays on screen. Long enough that someone
/// glancing across the window catches it, short enough that it doesn't
/// linger as clutter. The CSS keyframe must match.
const INTRO_DURATION_MS = 5000;

/// Tiny "copied" chip that fades in/out near the bottom of the sidebar
/// whenever `copyToClipboard` succeeds anywhere in the app. The
/// component is presentation-only: it subscribes to the
/// `rt:clipboard-copy` window event, schedules a self-clear, and renders
/// nothing the rest of the time. Pop-out windows mount their own React
/// tree (no Sidebar), so the pulse only appears in the main window —
/// which is the surface the user is looking at when triggering most
/// copies anyway.
///
/// On mount the chip flashes a one-shot intro so a first-time user
/// learns where to look. A real copy fired during the intro pre-empts it
/// and runs the normal pulse animation.
export default function CopyPulse() {
  const [pulse, setPulse] = useState<PulseState | null>(null);
  const [intro, setIntro] = useState(true);

  useEffect(() => {
    let counter = 0;
    let clearTimer: number | null = null;
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<ClipboardCopyDetail>).detail;
      counter += 1;
      // Real copy wins over the intro — dismiss the intro chip so the
      // pulse animation has a clean canvas to play on.
      setIntro(false);
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

  useEffect(() => {
    if (!intro) return;
    const t = window.setTimeout(() => setIntro(false), INTRO_DURATION_MS);
    return () => window.clearTimeout(t);
  }, [intro]);

  if (pulse !== null) {
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

  if (intro) {
    return (
      <div
        className="copy-pulse-host"
        aria-hidden="true"
        data-testid="copy-pulse-intro"
      >
        <div className="copy-pulse is-intro">
          <span className="copy-pulse-check" aria-hidden="true">
            ✓
          </span>
          <span className="copy-pulse-label">copies appear here</span>
        </div>
      </div>
    );
  }

  return null;
}
