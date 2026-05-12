/// Module-scoped queue of session ids that should receive xterm keyboard
/// focus on their next [`Terminal`](../components/Terminal.tsx) mount.
///
/// Producers (App after a spawn intent resolves, pop-out windows on first
/// render) call `markForAutoFocus`. The Terminal component calls
/// `consumeAutoFocus` exactly once per mount and focuses iff true.
///
/// Module-level state instead of prop-drilling through
/// App → GridRenderer → PaneChrome → SessionPane → Terminal: only the
/// Terminal cares about the signal, the routing path is fixed, and each
/// Tauri webview window runs its own JS context (so the set is naturally
/// per-window — main vs pop-out don't share state, which is what we want).
const pending = new Set<string>();

export function markForAutoFocus(sessionId: string): void {
  pending.add(sessionId);
}

/// Returns true (and clears the mark) iff the session was queued for
/// auto-focus. Safe to call multiple times; only the first call after a
/// matching `markForAutoFocus` returns true.
export function consumeAutoFocus(sessionId: string): boolean {
  return pending.delete(sessionId);
}
