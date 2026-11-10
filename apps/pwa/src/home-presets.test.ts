import { expect, test } from "vitest";

import { joinPath, phoneLaunch } from "./home-presets";
import type { AgentDescriptor, PresetEntry, PresetVariable, RepoEntry } from "./protocol";

const ALPHA: RepoEntry = { id: "a", name: "alpha", path: "C:\\dev\\alpha", default_branch: null };

/** A preset from repo `a` whose one source is a folder of plans. */
function preset(overrides: Partial<PresetEntry> = {}): PresetEntry {
  return {
    id: "p1",
    name: "Plans",
    source_repo_id: "a",
    prompt_sources: [{ kind: "folder", relative_path: "docs/plans" }],
    prompt_template: "{prompt}",
    branch_template: "plan-{index}",
    tab_grouping: { kind: "none" },
    injector: { steps: [] },
    ...overrides,
  } as PresetEntry;
}

function variable(overrides: Partial<PresetVariable>): PresetVariable {
  return { name: "v", label: "V", kind: { kind: "text" }, ...overrides };
}

/** The agent `alpha-agent`, whose `mode: bypass` choice skips its permission prompts. */
const AGENTS: readonly AgentDescriptor[] = [
  { id: "alpha-agent", bypass: { field: "mode", value: "bypass" } } as unknown as AgentDescriptor,
];

const INLINE_ONLY = preset({
  prompt_sources: [{ kind: "inline" }],
  prompt_template: "Run the checks",
});

test("a folder source qualifies with the repo path joined to its relative path", () => {
  expect(phoneLaunch(preset(), [ALPHA], AGENTS)).toEqual({
    kind: "folder",
    path: "C:\\dev\\alpha\\docs\\plans",
  });
});

test("an inline source whose template has no {prompt} qualifies with the preset's name", () => {
  expect(phoneLaunch(INLINE_ONLY, [ALPHA], AGENTS)).toEqual({ kind: "inline", prompts: ["Plans"] });
});

test("an inline source whose template has {prompt} does not qualify", () => {
  const typed = preset({
    prompt_sources: [{ kind: "inline" }],
    prompt_template: "Do {prompt} now",
  });
  expect(phoneLaunch(typed, [ALPHA], AGENTS)).toBeNull();
});

test("a file source alone does not qualify", () => {
  expect(phoneLaunch(preset({ prompt_sources: [{ kind: "file" }] }), [ALPHA], AGENTS)).toBeNull();
});

test("a github_issues source alone does not qualify", () => {
  const issues = preset({ prompt_sources: [{ kind: "github_issues" }] });
  expect(phoneLaunch(issues, [ALPHA], AGENTS)).toBeNull();
});

test("the first qualifying source wins over a later one", () => {
  const both = preset({
    prompt_sources: [
      { kind: "file" },
      { kind: "inline" },
      { kind: "folder", relative_path: "docs/plans" },
    ],
    prompt_template: "Run the checks",
  });
  expect(phoneLaunch(both, [ALPHA], AGENTS)).toEqual({ kind: "inline", prompts: ["Plans"] });
});

test("a variable asked at launch sends the preset to the desktop", () => {
  const asked = preset({ variables: [variable({ prompt_at_launch: true })] });
  expect(phoneLaunch(asked, [ALPHA], AGENTS)).toBeNull();
});

test("a script variable sends the preset to the desktop", () => {
  const script = preset({ variables: [variable({ kind: { kind: "script", cmd: "gh" } })] });
  expect(phoneLaunch(script, [ALPHA], AGENTS)).toBeNull();
});

test("a variable of a kind this build does not know sends the preset to the desktop", () => {
  const unknown = preset({
    variables: [variable({ kind: { kind: "later_kind" } as unknown as PresetVariable["kind"] })],
  });
  expect(phoneLaunch(unknown, [ALPHA], AGENTS)).toBeNull();
});

test("variables that resolve from their defaults are allowed", () => {
  const quiet = preset({
    variables: [
      variable({ name: "a" }),
      variable({ name: "b", kind: { kind: "env_var", name: "HOME" } }),
      variable({ name: "c", kind: { kind: "literal_path", path: "{repo_root}/log" } }),
      variable({ name: "d", kind: { kind: "toggle", value: "x" } }),
      variable({ name: "e", kind: { kind: "file_path" } }),
      variable({ name: "f", kind: { kind: "folder_path" } }),
    ],
  });
  expect(phoneLaunch(quiet, [ALPHA], AGENTS)).not.toBeNull();
});

test("agent options holding the agent's bypass choice send the preset to the desktop", () => {
  const bypass = preset({ agent: { agent: "alpha-agent", options: { mode: "bypass" } } });
  expect(phoneLaunch(bypass, [ALPHA], AGENTS)).toBeNull();
});

test("agent options without the bypass choice are allowed", () => {
  const plain = preset({ agent: { agent: "alpha-agent", options: { mode: "ask" } } });
  expect(phoneLaunch(plain, [ALPHA], AGENTS)).not.toBeNull();
});

test("agent options before the agent descriptors arrive send the preset to the desktop", () => {
  const plain = preset({ agent: { agent: "alpha-agent", options: { mode: "ask" } } });
  expect(phoneLaunch(plain, [ALPHA], null)).toBeNull();
  expect(phoneLaunch(preset(), [ALPHA], null)).not.toBeNull();
});

test("an unknown source repo disables a folder preset but not an inline one", () => {
  expect(phoneLaunch(preset({ source_repo_id: "gone" }), [ALPHA], AGENTS)).toBeNull();
  expect(phoneLaunch({ ...INLINE_ONLY, source_repo_id: "gone" }, [ALPHA], AGENTS)).toEqual({
    kind: "inline",
    prompts: ["Plans"],
  });
});

test("join_path: a drive-letter base joins with backslashes", () => {
  expect(joinPath("C:\\dev\\alpha", "docs/plans")).toBe("C:\\dev\\alpha\\docs\\plans");
});

test("join_path: a UNC base joins with backslashes", () => {
  expect(joinPath("\\\\host\\share\\alpha", "docs/plans")).toBe(
    "\\\\host\\share\\alpha\\docs\\plans",
  );
});

test("join_path: a POSIX base joins with slashes", () => {
  expect(joinPath("/home/me/alpha", "docs\\plans")).toBe("/home/me/alpha/docs/plans");
});

test("join_path: stray separators at the seam are dropped", () => {
  expect(joinPath("C:\\dev\\alpha\\", "\\docs/plans")).toBe("C:\\dev\\alpha\\docs\\plans");
  expect(joinPath("/home/me/alpha//", "/docs")).toBe("/home/me/alpha/docs");
});

test("join_path: an empty relative path gives the base", () => {
  expect(joinPath("C:\\dev\\alpha\\", "")).toBe("C:\\dev\\alpha");
  expect(joinPath("/home/me/alpha", "/")).toBe("/home/me/alpha");
});
