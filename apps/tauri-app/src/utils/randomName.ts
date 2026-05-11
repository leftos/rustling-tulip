/// Adjective/noun pair generator used to seed a unique branch name when the
/// user opts to create a worktree but hasn't picked a name yet. Format:
/// `wt/<adjective>-<noun>`. The pool is small (16×16 = 256 combos) — that's
/// plenty for one user iterating manually, and the field stays editable.

const ADJECTIVES = [
  "sleepy",
  "brave",
  "calm",
  "eager",
  "gentle",
  "happy",
  "jolly",
  "kind",
  "lively",
  "mighty",
  "nimble",
  "polite",
  "quick",
  "silly",
  "witty",
  "zen",
] as const;

const NOUNS = [
  "otter",
  "panda",
  "lynx",
  "fox",
  "hawk",
  "raven",
  "tiger",
  "whale",
  "koala",
  "ferret",
  "badger",
  "marten",
  "lemur",
  "weasel",
  "gibbon",
  "gecko",
] as const;

function pick<T>(arr: readonly T[]): T {
  // Non-empty by construction — both arrays are 16 literals above.
  return arr[Math.floor(Math.random() * arr.length)] as T;
}

export function randomWorktreeBranchName(): string {
  return `wt/${pick(ADJECTIVES)}-${pick(NOUNS)}`;
}
