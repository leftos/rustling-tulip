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
