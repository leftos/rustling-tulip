/// Clipboard write with a side-channel "I just copied something" event.
///
/// All copy paths in the app (terminal selection auto-copy, Ctrl+C with
/// selection, Ctrl+Shift+C, "copy path" buttons in the daemon footer and
/// sidebar) route through this helper so the sidebar's CopyPulse can
/// fire a single short visual confirmation regardless of which surface
/// initiated the copy.
///
/// Returns the writeText promise so callers that already chain on it
/// (DaemonFooter's `setCopied(key)` toast) keep working. Failures are
/// surfaced through the promise — the event only fires on a successful
/// write so the pulse doesn't lie about state.

const EVENT_NAME = "rt:clipboard-copy";

export interface ClipboardCopyDetail {
  /// Short tag describing what was copied. Used by the pulse to show a
  /// matching label (e.g. "copied" vs "copied path"). Free-form short
  /// string — keep it under ~16 chars for the chip to render cleanly.
  source: string;
  /// Length of the copied text. Surfaced in the pulse tooltip so power
  /// users can sanity-check the copy without pasting.
  length: number;
}

export async function copyToClipboard(
  text: string,
  source: string,
): Promise<void> {
  await navigator.clipboard.writeText(text);
  window.dispatchEvent(
    new CustomEvent<ClipboardCopyDetail>(EVENT_NAME, {
      detail: { source, length: text.length },
    }),
  );
}

export { EVENT_NAME as CLIPBOARD_COPY_EVENT };
