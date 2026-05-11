import { useEffect, type RefObject } from "react";

/**
 * Clamp a context-menu coordinate so the menu stays in the viewport. Without
 * this, right-clicking near the bottom-right corner can render the menu
 * partially off-screen (the browser clips it but doesn't reflow).
 *
 * `size` is the menu's estimated extent along the axis. `axis` defaults to
 * `"width"`; pass `"height"` for the vertical clamp. The 12px margin keeps
 * the menu from hugging the window edge.
 */
export function clampMenuCoord(
  raw: number,
  size: number,
  axis: "width" | "height" = "width",
): number {
  const windowSize =
    axis === "width" ? window.innerWidth : window.innerHeight;
  const max = Math.max(12, windowSize - size - 12);
  return Math.min(Math.max(12, raw), max);
}

/**
 * Subscribe to the Escape key while `enabled` is true. The handler runs at
 * the document level so it works regardless of which element has focus (the
 * common case in modals where the user has clicked an input, then wants to
 * bail out).
 *
 * Pass `enabled: false` to bypass without unmounting — useful for nested
 * dismissable layers where only the topmost should respond.
 */
export function useEscape(onEscape: () => void, enabled: boolean = true): void {
  useEffect(() => {
    if (!enabled) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onEscape();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [enabled, onEscape]);
}

/**
 * Focus the element behind `ref` once on mount. Used by modal dialogs to land
 * the keyboard cursor on the first sensible control (a Cancel button is the
 * safe default for destructive-action modals; a primary input for forms).
 * No-op if the ref is empty when the effect runs.
 */
export function useAutoFocus(
  ref: RefObject<HTMLElement | null>,
  enabled: boolean = true,
): void {
  useEffect(() => {
    if (!enabled) return;
    ref.current?.focus();
  }, [enabled, ref]);
}

/**
 * Capture the element that had focus when this component mounted, and
 * restore focus to it on unmount. Keyboard users opening a modal land on
 * a controlled focus target, do their thing, dismiss, and resume from
 * where they were rather than landing on `<body>` with no path back.
 *
 * Skips the restore if the captured element has been removed from the
 * document by the time the modal unmounts (rare — e.g. modal-open
 * dropped the source button).
 */
export function useFocusReturn(): void {
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    return () => {
      if (!previouslyFocused || !document.body.contains(previouslyFocused)) {
        return;
      }
      // Defer to the next tick so React's unmount-phase DOM teardown
      // doesn't clobber focus the same frame.
      window.setTimeout(() => previouslyFocused.focus(), 0);
    };
  }, []);
}

export interface KeyboardShortcut {
  /// Key name as reported by KeyboardEvent.key (e.g. "t", "Tab", "1").
  /// Compared case-insensitively for letters.
  key: string;
  /// Require Ctrl (Windows/Linux) or Meta (macOS). Defaults to true since
  /// most app shortcuts are modified to avoid typing collisions.
  ctrlOrMeta?: boolean;
  /// Require Shift. Defaults to false.
  shift?: boolean;
  handler: () => void;
}

/// Returns true if the event target is an editable surface — a regular
/// form input, a textarea, a contenteditable element, or the xterm.js
/// terminal viewport. App-level shortcuts skip these so typing inside an
/// input/terminal isn't hijacked.
function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
  if (target.isContentEditable) return true;
  if (target.closest(".xterm")) return true;
  return false;
}

/**
 * Bind a set of document-level keyboard shortcuts. Shortcuts skip when
 * focus is in an editable surface (input, textarea, contenteditable,
 * xterm) so typing doesn't fire them by accident. preventDefault fires
 * only after a match.
 *
 * Pass an empty array to disable without unmounting. The dependency on
 * `shortcuts.length` ensures the effect re-subscribes when the binding
 * list changes shape; for stable bindings prefer `useMemo` at the call
 * site.
 */
export function useKeyboardShortcuts(shortcuts: KeyboardShortcut[]): void {
  useEffect(() => {
    if (shortcuts.length === 0) return;
    const onKey = (e: KeyboardEvent) => {
      if (isEditableTarget(e.target)) return;
      for (const sc of shortcuts) {
        const wantCtrl = sc.ctrlOrMeta !== false;
        const wantShift = sc.shift === true;
        const hasCtrl = e.ctrlKey || e.metaKey;
        if (wantCtrl !== hasCtrl) continue;
        if (wantShift !== e.shiftKey) continue;
        if (sc.key.toLowerCase() !== e.key.toLowerCase()) continue;
        e.preventDefault();
        sc.handler();
        return;
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [shortcuts]);
}
