// The Quick dock's one-tap replies: the agent and shell groups collie ships
// (`web/src/lib/quick-replies.ts`, MIT-licensed), copied as data.

/** A section of the Quick dock: its label, and the texts it offers. */
export interface QuickReplyGroup {
  /** The section label shown above the replies. */
  readonly title: string;
  /** The texts sent, each typed into the session and submitted. */
  readonly items: readonly string[];
}

// Every LLM harness reads "yes" and "continue" the same way.
const AGENT: readonly QuickReplyGroup[] = [
  { title: "confirm", items: ["yes", "no"] },
  {
    title: "common",
    items: ["continue", "commit and push", "retry", "skip", "drastically simplify"],
  },
];

// A bare shell has no turn to continue or skip: only the classic y/n pair.
const SHELL: readonly QuickReplyGroup[] = [{ title: "confirm", items: ["y", "n"] }];

/** The groups a pane gets: the shared agent set for an agent, the shell pair otherwise. */
export function quickRepliesFor(isAgent: boolean): readonly QuickReplyGroup[] {
  return isAgent ? AGENT : SHELL;
}

/** The bytes a quick reply types: its text, then `\r`, which submits it. */
export function quickReplyBytes(text: string): string {
  return `${text}\r`;
}
