import { describe, expect, it } from "vitest";
import type { BranchFate, MemberBranchFate } from "../types";
import { describeMemberFate, discardChoice } from "./branchFate";

function member(fate: BranchFate, repoName = "repo1"): MemberBranchFate {
  return {
    repo_id: `${repoName}-id`,
    repo_name: repoName,
    branch: "wt/brave-otter",
    fate,
  };
}

/// A tag/value the daemon could grow that this build has never seen. Cast
/// because the TS union only lists today's variants on purpose.
function unknownFate(raw: unknown): BranchFate {
  return raw as BranchFate;
}

describe("describeMemberFate", () => {
  it("names the target for an ancestry-merged branch", () => {
    expect(
      describeMemberFate(
        member({ kind: "will_delete", into: "origin/main", via: "ancestry" }),
      ),
    ).toBe("already merged into origin/main");
  });

  it("flags a patch-equivalent land as cherry-picked or rebased", () => {
    expect(
      describeMemberFate(
        member({
          kind: "will_delete",
          into: "origin/main",
          via: "patch_equivalent",
        }),
      ),
    ).toBe("already in origin/main (cherry-picked or rebased)");
  });

  it("falls back to a bare 'already in' for an unknown merge evidence", () => {
    expect(
      describeMemberFate(
        member(
          unknownFate({
            kind: "will_delete",
            into: "origin/main",
            via: "bisected",
          }),
        ),
      ),
    ).toBe("already in origin/main");
  });

  it("lists every ref a kept branch was measured against", () => {
    expect(
      describeMemberFate(
        member({
          kind: "kept_by_default",
          unique_commits: 3,
          checked_against: ["origin/main", "main"],
        }),
      ),
    ).toBe("3 commits not in origin/main, main");
  });

  it("uses the singular for a single unique commit", () => {
    expect(
      describeMemberFate(
        member({
          kind: "kept_by_default",
          unique_commits: 1,
          checked_against: ["main"],
        }),
      ),
    ).toBe("1 commit not in main");
  });

  it("says 'anywhere the daemon checked' when no target resolved", () => {
    expect(
      describeMemberFate(
        member({
          kind: "kept_by_default",
          unique_commits: 2,
          checked_against: [],
        }),
      ),
    ).toBe("2 commits not found anywhere the daemon checked");
  });

  it("admits ignorance when the commit count is unknown", () => {
    expect(
      describeMemberFate(
        member({
          kind: "kept_by_default",
          unique_commits: null,
          checked_against: ["main"],
        }),
      ),
    ).toBe("couldn't determine whether its commits landed");
  });

  it("explains each untouched reason", () => {
    expect(
      describeMemberFate(
        member({ kind: "untouched", reason: "external_worktree" }),
      ),
    ).toBe("not managed by rustling-tulip; left alone");
    expect(
      describeMemberFate(
        member({ kind: "untouched", reason: "checked_out_elsewhere" }),
      ),
    ).toBe("checked out in another worktree; left alone");
    expect(
      describeMemberFate(
        member({ kind: "untouched", reason: "branch_missing" }),
      ),
    ).toBe("branch no longer exists");
  });

  it("falls back to 'left alone' for an unknown untouched reason", () => {
    expect(
      describeMemberFate(
        member(unknownFate({ kind: "untouched", reason: "locked_by_hook" })),
      ),
    ).toBe("left alone");
  });

  it("describes an unrecognized fate kind without claiming an outcome", () => {
    expect(
      describeMemberFate(member(unknownFate({ kind: "will_archive" }))),
    ).toBe("unknown state; left alone unless you choose delete");
  });
});

describe("discardChoice", () => {
  it("offers the single delete button when every branch already landed", () => {
    expect(
      discardChoice([
        member({ kind: "will_delete", into: "main", via: "ancestry" }),
        member(
          { kind: "will_delete", into: "main", via: "patch_equivalent" },
          "repo2",
        ),
      ]),
    ).toEqual({ kind: "delete_all" });
  });

  it("asks for a choice when a branch holds unlanded work", () => {
    expect(
      discardChoice([
        member({
          kind: "kept_by_default",
          unique_commits: 4,
          checked_against: ["main"],
        }),
      ]),
    ).toEqual({ kind: "choose", lostCommits: 4 });
  });

  it("removes the worktree only when no branch is deletable", () => {
    expect(
      discardChoice([
        member({ kind: "untouched", reason: "external_worktree" }),
        member({ kind: "untouched", reason: "branch_missing" }, "repo2"),
      ]),
    ).toEqual({ kind: "worktree_only" });
  });

  it("treats an empty member list as worktree-only", () => {
    expect(discardChoice([])).toEqual({ kind: "worktree_only" });
  });

  it("keeps delete_all when untouched members ride along", () => {
    expect(
      discardChoice([
        member({ kind: "will_delete", into: "main", via: "ancestry" }),
        member(
          { kind: "untouched", reason: "checked_out_elsewhere" },
          "repo2",
        ),
      ]),
    ).toEqual({ kind: "delete_all" });
  });

  it("one kept member outvotes any number of merged ones", () => {
    expect(
      discardChoice([
        member({ kind: "will_delete", into: "main", via: "ancestry" }),
        member(
          {
            kind: "kept_by_default",
            unique_commits: 2,
            checked_against: ["main"],
          },
          "repo2",
        ),
      ]),
    ).toEqual({ kind: "choose", lostCommits: 2 });
  });

  it("sums unique commits across kept members", () => {
    expect(
      discardChoice([
        member({
          kind: "kept_by_default",
          unique_commits: 2,
          checked_against: ["main"],
        }),
        member(
          {
            kind: "kept_by_default",
            unique_commits: 5,
            checked_against: ["main"],
          },
          "repo2",
        ),
      ]),
    ).toEqual({ kind: "choose", lostCommits: 7 });
  });

  it("propagates null when one kept member's count is unknown", () => {
    expect(
      discardChoice([
        member({
          kind: "kept_by_default",
          unique_commits: 2,
          checked_against: ["main"],
        }),
        member(
          {
            kind: "kept_by_default",
            unique_commits: null,
            checked_against: ["main"],
          },
          "repo2",
        ),
      ]),
    ).toEqual({ kind: "choose", lostCommits: null });
  });

  it("an unknown fate kind forces the choice with an unknown count", () => {
    expect(discardChoice([member(unknownFate({ kind: "will_archive" }))])).toEqual(
      { kind: "choose", lostCommits: null },
    );
  });

  it("an unknown kind hides an otherwise countable total", () => {
    expect(
      discardChoice([
        member({
          kind: "kept_by_default",
          unique_commits: 3,
          checked_against: ["main"],
        }),
        member(unknownFate({ kind: "will_archive" }), "repo2"),
      ]),
    ).toEqual({ kind: "choose", lostCommits: null });
  });
});
