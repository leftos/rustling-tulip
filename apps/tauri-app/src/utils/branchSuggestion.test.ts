import { describe, expect, it } from "vitest";
import type { DaemonMessage, SuggestTarget } from "../types";
import {
  matchesSuggestion,
  suggestTargetFor,
  suggestTargetKey,
} from "./branchSuggestion";

function suggestion(
  target: SuggestTarget,
  name = "wt/brave-otter",
): DaemonMessage {
  return { type: "branch_name_suggestion", target, name };
}

describe("suggestTargetFor", () => {
  it("maps a repo selection to the repo variant", () => {
    expect(suggestTargetFor({ kind: "repo", id: "r1" })).toEqual({
      kind: "repo",
      repo_id: "r1",
    });
  });

  it("maps a workspace selection to the workspace variant", () => {
    expect(suggestTargetFor({ kind: "workspace", id: "w1" })).toEqual({
      kind: "workspace",
      workspace_id: "w1",
    });
  });
});

describe("suggestTargetKey", () => {
  it("keys a repo target as repo:<id>", () => {
    expect(suggestTargetKey({ kind: "repo", repo_id: "r1" })).toBe("repo:r1");
  });

  it("keys a workspace target as workspace:<id>", () => {
    expect(suggestTargetKey({ kind: "workspace", workspace_id: "w1" })).toBe(
      "workspace:w1",
    );
  });

  it("round-trips a selection through suggestTargetFor", () => {
    expect(suggestTargetKey(suggestTargetFor({ kind: "repo", id: "r1" }))).toBe(
      "repo:r1",
    );
    expect(
      suggestTargetKey(suggestTargetFor({ kind: "workspace", id: "w1" })),
    ).toBe("workspace:w1");
  });
});

describe("matchesSuggestion", () => {
  it("returns the name for the repo target that asked", () => {
    expect(
      matchesSuggestion(
        suggestion({ kind: "repo", repo_id: "r1" }, "wt/quick-lynx"),
        "repo:r1",
      ),
    ).toBe("wt/quick-lynx");
  });

  it("returns the name for the workspace target that asked", () => {
    expect(
      matchesSuggestion(
        suggestion({ kind: "workspace", workspace_id: "w1" }, "wt/zen-gecko"),
        "workspace:w1",
      ),
    ).toBe("wt/zen-gecko");
  });

  it("rejects a reply for a different id", () => {
    expect(
      matchesSuggestion(suggestion({ kind: "repo", repo_id: "r2" }), "repo:r1"),
    ).toBeNull();
  });

  it("rejects a reply for the other target kind with the same id", () => {
    expect(
      matchesSuggestion(
        suggestion({ kind: "workspace", workspace_id: "r1" }),
        "repo:r1",
      ),
    ).toBeNull();
  });

  it("rejects a message that is not a suggestion", () => {
    const other: DaemonMessage = { type: "session_removed", session_id: "s1" };
    expect(matchesSuggestion(other, "repo:r1")).toBeNull();
  });
});
