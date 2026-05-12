/// Bundled coding fonts shipped with the app. The side-effect CSS imports
/// pull the .woff2 binaries through Vite's asset pipeline and emit the
/// matching @font-face declarations, so the families below become usable
/// (via `document.fonts.load(...)` or plain font-family CSS) by the time
/// `main.tsx` has run.
///
/// Weights 400 and 700 only — that's what the terminal needs for the
/// regular + bold pair. Heavier or lighter weights bloat the bundle
/// without any current consumer.
import "@fontsource/fira-code/400.css";
import "@fontsource/fira-code/700.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/700.css";
import "@fontsource/cascadia-code/400.css";
import "@fontsource/cascadia-code/700.css";

export interface BundledFont {
  /// The CSS font-family value, suitable for xterm.js / @font-face.
  family: string;
  /// User-facing label in the Settings dropdown.
  label: string;
}

export const BUNDLED_FONTS: readonly BundledFont[] = [
  { family: "Geist Mono", label: "Geist Mono (default)" },
  { family: "Fira Code", label: "Fira Code" },
  { family: "JetBrains Mono", label: "JetBrains Mono" },
  { family: "Cascadia Code", label: "Cascadia Code" },
] as const;

/// True iff a candidate family matches one of our bundled entries.
/// Lets the Settings picker render `system` / `custom` fonts under a
/// separate optgroup without false positives.
export function isBundledFont(family: string): boolean {
  return BUNDLED_FONTS.some((f) => f.family === family);
}
