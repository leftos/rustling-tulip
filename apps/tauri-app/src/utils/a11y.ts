import { useEffect, type RefObject } from "react";

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
