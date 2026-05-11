import { useEffect } from "react";

export type ToastSeverity = "error" | "warning" | "info";

export interface ToastEntry {
  id: string;
  /// Optional dedup key used by `upsertToast` to update a toast in place
  /// instead of pushing a new one. Same key → replace fields; absent →
  /// always push. Distinct from `id` so React's stable element key
  /// doesn't change across updates (no remount, no animation reset).
  key?: string;
  /// Bumped by `upsertToast` on every update. The Toast component keys
  /// its auto-dismiss `useEffect` on this so an in-place update restarts
  /// the dismiss timer — a streaming progress toast doesn't vanish mid-flight.
  generation?: number;
  severity: ToastSeverity;
  message: string;
  detail?: string;
  /// When `true`, the toast doesn't auto-dismiss. Useful for in-flight
  /// progress that needs to stay until explicitly resolved (e.g.
  /// preset launch progress, where the final `done` update flips this
  /// back to false so the toast fades naturally).
  sticky?: boolean;
}

interface Props {
  toasts: ToastEntry[];
  onDismiss: (id: string) => void;
  /// Auto-dismiss timeout in ms. Default 8 s. Each toast schedules its own
  /// setTimeout on mount; clearing happens on manual dismiss or unmount.
  autoDismissMs?: number;
}

/**
 * Bottom-right stacked toast container. New toasts append to the array, so
 * they render last and appear closest to the corner; older toasts sit above
 * them. Two severity levels: error (red) and warning (amber).
 */
export default function ErrorToast({
  toasts,
  onDismiss,
  autoDismissMs = 8000,
}: Props) {
  if (toasts.length === 0) return null;
  return (
    <div className="error-toast-container" data-testid="error-toast-container">
      {toasts.map((t) => (
        <Toast
          key={t.id}
          toast={t}
          autoDismissMs={autoDismissMs}
          onDismiss={onDismiss}
        />
      ))}
    </div>
  );
}

function Toast({
  toast,
  autoDismissMs,
  onDismiss,
}: {
  toast: ToastEntry;
  autoDismissMs: number;
  onDismiss: (id: string) => void;
}) {
  // Skip the auto-dismiss timer when the toast is sticky (in-flight
  // progress). When sticky is flipped off (e.g. preset launch completes
  // with a final `done` upsert), the effect's deps change and a fresh
  // timer fires. Generation also retriggers so in-place content updates
  // reset the dismiss clock.
  useEffect(() => {
    if (toast.sticky) return;
    const timer = window.setTimeout(() => onDismiss(toast.id), autoDismissMs);
    return () => window.clearTimeout(timer);
  }, [toast.id, toast.generation, toast.sticky, autoDismissMs, onDismiss]);

  return (
    <div
      className={`error-toast error-toast-${toast.severity}`}
      data-testid="error-toast"
      data-toast-severity={toast.severity}
      role={toast.severity === "error" ? "alert" : "status"}
    >
      <div className="error-toast-body">
        <div className="error-toast-message">{toast.message}</div>
        {toast.detail !== undefined && toast.detail.length > 0 && (
          <div className="error-toast-detail">{toast.detail}</div>
        )}
      </div>
      <button
        type="button"
        className="error-toast-close"
        onClick={() => onDismiss(toast.id)}
        aria-label="Dismiss notification"
        data-testid="error-toast-close"
      >
        ×
      </button>
    </div>
  );
}
