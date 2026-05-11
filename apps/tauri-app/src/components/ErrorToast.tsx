import { useEffect } from "react";

export type ToastSeverity = "error" | "warning";

export interface ToastEntry {
  id: string;
  severity: ToastSeverity;
  message: string;
  detail?: string;
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
  useEffect(() => {
    const timer = window.setTimeout(() => onDismiss(toast.id), autoDismissMs);
    return () => window.clearTimeout(timer);
  }, [toast.id, autoDismissMs, onDismiss]);

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
