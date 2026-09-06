import { browser } from "@wdio/globals";

/**
 * A dialog handle either way it is held: the chainable `browser.$` returns,
 * or the Element an `await` on it produces. Specs use both spellings.
 */
type DialogLike =
  | ReturnType<typeof browser.$>
  | Awaited<ReturnType<typeof browser.$>>;

/** Branch-name inputs the spawn dialog renders, single-repo form first. */
const BRANCH_FIELDS = [
  '[data-testid="spawn-single-branch"]',
  '[data-testid="spawn-workspace-branch"]',
] as const;

const SUGGESTING = '[data-testid="spawn-branch-suggesting"]';

/**
 * Wait until the dialog's branch-name field holds a name that is safe to read
 * or overwrite.
 *
 * In worktree mode the field starts empty and the daemon names the branch —
 * only it can check the name against every member repo's refs — so a spec that
 * reads (or types into) the field the moment the dialog opens either sees the
 * empty placeholder or has its own text overwritten when the reply lands. This
 * waits for both halves of "the round trip is over": the pending indicator is
 * gone and the field is non-empty.
 *
 * In-place mode never asks for a suggestion; the field is seeded with the
 * default branch synchronously, so this returns as soon as it is read.
 */
export async function waitForBranchSuggestion(
  dialog: DialogLike,
  timeoutMs = 15_000,
): Promise<void> {
  let last = "nothing observed yet";
  await browser
    .waitUntil(
      async () => {
        if (await (await dialog.$(SUGGESTING)).isExisting()) {
          last = "the daemon is still picking a name";
          return false;
        }
        for (const selector of BRANCH_FIELDS) {
          const field = await dialog.$(selector);
          if (!(await field.isExisting())) continue;
          const value = await field.getValue();
          if (value.length > 0) return true;
          last = `${selector} is still empty`;
          return false;
        }
        last = "no branch field is rendered";
        return false;
      },
      { timeout: timeoutMs },
    )
    .catch(() => {
      throw new Error(
        `spawn dialog never settled on a branch name: ${last}`,
      );
    });
}
