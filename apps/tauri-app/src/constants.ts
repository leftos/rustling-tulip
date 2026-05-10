// Available Claude model IDs for the spawn dialog.
//
// Keep in sync with the Anthropic public model lineup. The daemon passes
// whatever string is sent here verbatim to `claude --model`, so unknown IDs
// will surface as a CLI error rather than a frontend bug.

export interface ClaudeModel {
  id: string;
  label: string;
}

export const CLAUDE_MODELS: ClaudeModel[] = [
  { id: "claude-opus-4-7", label: "Opus 4.7" },
  { id: "claude-sonnet-4-6", label: "Sonnet 4.6" },
  { id: "claude-haiku-4-5-20251001", label: "Haiku 4.5" },
];
