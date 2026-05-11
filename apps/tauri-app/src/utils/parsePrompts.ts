/**
 * Split a prompts file (or inline textarea) into individual prompts.
 * Mirrors the Rust parser in crates/daemon/src/presets.rs and the original
 * yaat extension (extension.js:14-43):
 *
 * - A leading `- ` or `-\t` (with optional indent) starts a single-line
 *   prompt that ends at the line break.
 * - Otherwise lines accumulate into a paragraph until a blank line ends
 *   the paragraph.
 * - Empty prompts (whitespace-only blocks) are dropped.
 *
 * The daemon re-parses files server-side, so this is purely for the
 * inline-source preview path. Behavior intentionally matches so what the
 * user sees in the preview is what the daemon will run.
 */
export function parsePrompts(content: string): string[] {
  const prompts: string[] = [];
  let current: string[] = [];

  const flush = () => {
    if (current.length === 0) return;
    const block = current.join("\n").trim();
    current = [];
    if (block.length > 0) prompts.push(block);
  };

  for (const raw of content.split("\n")) {
    const line = raw.replace(/\r$/, "");
    const trimmedStart = line.replace(/^\s+/, "");
    const bulletMatch = trimmedStart.match(/^-[ \t](.*)$/);
    if (bulletMatch) {
      flush();
      const text = bulletMatch[1]?.trim() ?? "";
      if (text.length > 0) prompts.push(text);
    } else if (line.trim() === "") {
      flush();
    } else {
      current.push(line);
    }
  }
  flush();
  return prompts;
}
