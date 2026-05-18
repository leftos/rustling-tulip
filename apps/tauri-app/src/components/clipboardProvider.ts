import type { IClipboardProvider } from "@xterm/addon-clipboard";
import { copyToClipboard } from "../utils/clipboard";

/// IClipboardProvider routing OSC 52 writes from in-terminal TUIs through
/// the app's standard `copyToClipboard` helper so the CopyPulse fires
/// exactly once per confirmed system-clipboard write. The addon decodes
/// the OSC 52 base64 payload before calling `writeText`, so `text` is
/// the final UTF-8 string ready to hand to navigator.clipboard.
///
/// `readText` resolves to the empty string instead of actually reading
/// the system clipboard: enabling OSC 52 reads would let any TUI running
/// in a session exfiltrate the user's clipboard contents (which often
/// hold passwords, tokens, or other secrets) via the OSC 52 query form.
/// Resolving '' rather than rejecting avoids the addon surfacing an
/// unhandled-promise-rejection warning — the TUI sees a quiet "clipboard
/// is empty" response and moves on.
export class RtClipboardProvider implements IClipboardProvider {
  public async writeText(_selection: string, text: string): Promise<void> {
    await copyToClipboard(text, "OSC 52");
  }

  public readText(_selection: string): Promise<string> {
    return Promise.resolve("");
  }
}
