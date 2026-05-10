import { useEffect, useState } from "react";
import type { DaemonClient } from "../api";
import type { DaemonMessage, RepoEntry } from "../types";

interface Props {
  repos: RepoEntry[];
  client: DaemonClient;
  onClose: () => void;
}

interface BranchesState {
  branches: string[];
  current: string | null;
}

export default function SpawnDialog({ repos, client, onClose }: Props) {
  const [repoId, setRepoId] = useState<string>(repos[0]?.id ?? "");
  const [branches, setBranches] = useState<BranchesState | null>(null);
  const [branchMode, setBranchMode] = useState<"existing" | "new">("existing");
  const [existingBranch, setExistingBranch] = useState("");
  const [newBranchName, setNewBranchName] = useState("");
  const [baseBranch, setBaseBranch] = useState("");
  const [skipPerms, setSkipPerms] = useState(true);

  useEffect(() => {
    if (!repoId) return;
    setBranches(null);
    client.send({ type: "list_branches", repo_id: repoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "branches" || detail.repo_id !== repoId) return;
      setBranches({
        branches: detail.branches,
        current: detail.current,
      });
      const fallback = detail.current ?? detail.branches[0] ?? "";
      setExistingBranch(fallback);
      setBaseBranch(detail.current ?? detail.branches[0] ?? "main");
    };
    window.addEventListener("rt:branches", handler);
    return () => window.removeEventListener("rt:branches", handler);
  }, [repoId, client]);

  const canSubmit =
    repoId &&
    (branchMode === "existing" ? existingBranch : newBranchName) &&
    (branchMode === "new" ? baseBranch : true);

  const submit = () => {
    if (!canSubmit) return;
    const branch =
      branchMode === "existing"
        ? { kind: "existing" as const, name: existingBranch }
        : {
            kind: "new_from_base" as const,
            name: newBranchName,
            base: baseBranch,
          };
    client.send({
      type: "spawn_session",
      label: null,
      target: { kind: "single", repo_id: repoId, branch },
      mode: "interactive",
      initial_prompt: null,
      dangerously_skip_permissions: skipPerms,
    });
    onClose();
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <header className="modal-header">
          <h2>Spawn session</h2>
          <button type="button" className="link" onClick={onClose}>
            ✕
          </button>
        </header>
        <div className="modal-body">
          <label className="field">
            <span>Repo</span>
            <select value={repoId} onChange={(e) => setRepoId(e.target.value)}>
              {repos.map((r) => (
                <option key={r.id} value={r.id}>
                  {r.name}
                </option>
              ))}
            </select>
          </label>

          <fieldset className="field">
            <legend>Branch</legend>
            <label className="radio">
              <input
                type="radio"
                checked={branchMode === "existing"}
                onChange={() => setBranchMode("existing")}
              />
              Existing
            </label>
            <label className="radio">
              <input
                type="radio"
                checked={branchMode === "new"}
                onChange={() => setBranchMode("new")}
              />
              New
            </label>
          </fieldset>

          {branchMode === "existing" ? (
            <label className="field">
              <span>Branch</span>
              <select
                value={existingBranch}
                onChange={(e) => setExistingBranch(e.target.value)}
                disabled={!branches}
              >
                {branches?.branches.map((b) => (
                  <option key={b} value={b}>
                    {b}
                    {b === branches?.current ? " (current)" : ""}
                  </option>
                ))}
              </select>
            </label>
          ) : (
            <>
              <label className="field">
                <span>New branch name</span>
                <input
                  type="text"
                  value={newBranchName}
                  onChange={(e) => setNewBranchName(e.target.value)}
                  placeholder="feat/whatever"
                />
              </label>
              <label className="field">
                <span>Base</span>
                <select
                  value={baseBranch}
                  onChange={(e) => setBaseBranch(e.target.value)}
                  disabled={!branches}
                >
                  {branches?.branches.map((b) => (
                    <option key={b} value={b}>
                      {b}
                    </option>
                  ))}
                </select>
              </label>
            </>
          )}

          <label className="checkbox">
            <input
              type="checkbox"
              checked={skipPerms}
              onChange={(e) => setSkipPerms(e.target.checked)}
            />
            <span>Pass --dangerously-skip-permissions</span>
          </label>
        </div>
        <footer className="modal-footer">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            disabled={!canSubmit}
            onClick={submit}
          >
            Spawn
          </button>
        </footer>
      </div>
    </div>
  );
}
