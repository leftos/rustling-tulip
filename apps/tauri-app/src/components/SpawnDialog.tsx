import { useEffect, useState } from "react";
import type { DaemonClient } from "../api";
import type {
  DaemonMessage,
  MemberSpawnPreview,
  RepoEntry,
  WorkspaceEntry,
} from "../types";

interface Props {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  onClose: () => void;
}

type Mode = "single" | "workspace";
type BranchMode = "existing" | "new";

interface BranchesState {
  branches: string[];
  current: string | null;
}

export default function SpawnDialog({
  repos,
  workspaces,
  client,
  onClose,
}: Props) {
  const [mode, setMode] = useState<Mode>(
    workspaces.length > 0 ? "workspace" : "single",
  );
  const [skipPerms, setSkipPerms] = useState(true);

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
          <fieldset className="field">
            <legend>Type</legend>
            <label className="radio">
              <input
                type="radio"
                checked={mode === "single"}
                onChange={() => setMode("single")}
              />
              Single repo
            </label>
            <label className="radio">
              <input
                type="radio"
                checked={mode === "workspace"}
                onChange={() => setMode("workspace")}
                disabled={workspaces.length === 0}
              />
              Workspace
            </label>
          </fieldset>

          {mode === "single" ? (
            <SingleForm
              repos={repos}
              client={client}
              skipPerms={skipPerms}
              onClose={onClose}
              header={
                <Footer
                  skipPerms={skipPerms}
                  onSkipPermsChange={setSkipPerms}
                />
              }
            />
          ) : (
            <WorkspaceForm
              workspaces={workspaces}
              client={client}
              skipPerms={skipPerms}
              onClose={onClose}
              header={
                <Footer
                  skipPerms={skipPerms}
                  onSkipPermsChange={setSkipPerms}
                />
              }
            />
          )}
        </div>
      </div>
    </div>
  );
}

function Footer({
  skipPerms,
  onSkipPermsChange,
}: {
  skipPerms: boolean;
  onSkipPermsChange: (v: boolean) => void;
}) {
  return (
    <label className="checkbox">
      <input
        type="checkbox"
        checked={skipPerms}
        onChange={(e) => onSkipPermsChange(e.target.checked)}
      />
      <span>Pass --dangerously-skip-permissions</span>
    </label>
  );
}

// ---------- single-repo form ----------

function SingleForm({
  repos,
  client,
  skipPerms,
  onClose,
  header,
}: {
  repos: RepoEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  onClose: () => void;
  header: React.ReactNode;
}) {
  const [repoId, setRepoId] = useState<string>(repos[0]?.id ?? "");
  const [branches, setBranches] = useState<BranchesState | null>(null);
  const [branchMode, setBranchMode] = useState<BranchMode>("existing");
  const [existingBranch, setExistingBranch] = useState("");
  const [newBranchName, setNewBranchName] = useState("");
  const [baseBranch, setBaseBranch] = useState("");

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
    <>
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
                {b === branches.current ? " (current)" : ""}
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

      {header}
      <div className="modal-footer-inline">
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
      </div>
    </>
  );
}

// ---------- workspace form ----------

function WorkspaceForm({
  workspaces,
  client,
  skipPerms,
  onClose,
  header,
}: {
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  onClose: () => void;
  header: React.ReactNode;
}) {
  const [workspaceId, setWorkspaceId] = useState<string>(
    workspaces[0]?.id ?? "",
  );
  const [branchName, setBranchName] = useState("");
  const [baseBranch, setBaseBranch] = useState("");
  const [preview, setPreview] = useState<MemberSpawnPreview[] | null>(null);
  const [previewBranch, setPreviewBranch] = useState<string | null>(null);

  useEffect(() => {
    setPreview(null);
    setPreviewBranch(null);
  }, [workspaceId, branchName, baseBranch]);

  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "workspace_spawn_preview" ||
        detail.workspace_id !== workspaceId ||
        detail.branch_name !== branchName
      ) {
        return;
      }
      setPreview(detail.per_member);
      setPreviewBranch(detail.branch_name);
    };
    window.addEventListener("rt:workspace_spawn_preview", handler);
    return () =>
      window.removeEventListener("rt:workspace_spawn_preview", handler);
  }, [workspaceId, branchName]);

  const requestPreview = () => {
    if (!workspaceId || !branchName) return;
    client.send({
      type: "preview_workspace_spawn",
      workspace_id: workspaceId,
      branch_name: branchName,
      base_branch: baseBranch || null,
    });
  };

  const canSpawn =
    !!preview && previewBranch === branchName && workspaceId && branchName;

  const submit = () => {
    if (!canSpawn) return;
    client.send({
      type: "spawn_session",
      label: null,
      target: {
        kind: "workspace",
        workspace_id: workspaceId,
        branch_name: branchName,
        base_branch: baseBranch || null,
      },
      mode: "interactive",
      initial_prompt: null,
      dangerously_skip_permissions: skipPerms,
    });
    onClose();
  };

  return (
    <>
      <label className="field">
        <span>Workspace</span>
        <select
          value={workspaceId}
          onChange={(e) => setWorkspaceId(e.target.value)}
        >
          {workspaces.map((w) => (
            <option key={w.id} value={w.id}>
              {w.name} ({w.member_repo_ids.length} repos)
            </option>
          ))}
        </select>
      </label>

      <label className="field">
        <span>Branch name (same across all members)</span>
        <input
          type="text"
          value={branchName}
          onChange={(e) => setBranchName(e.target.value)}
          placeholder="feat/whatever"
        />
      </label>

      <label className="field">
        <span>Base when creating new (optional)</span>
        <input
          type="text"
          value={baseBranch}
          onChange={(e) => setBaseBranch(e.target.value)}
          placeholder="main"
        />
      </label>

      <div className="modal-footer-inline">
        <button type="button" onClick={requestPreview} disabled={!branchName}>
          Preview
        </button>
      </div>

      {preview && (
        <div className="preview-table">
          <table>
            <thead>
              <tr>
                <th>Repo</th>
                <th>Branch</th>
                <th>Action</th>
                <th>Worktree</th>
              </tr>
            </thead>
            <tbody>
              {preview.map((m) => (
                <tr key={m.repo_id}>
                  <td>{m.repo_name}</td>
                  <td>{branchName}</td>
                  <td>
                    {m.branch_exists ? (
                      <span className="badge badge-ok">reuse</span>
                    ) : (
                      <span className="badge badge-warn">
                        new from {m.effective_base ?? "main"}
                      </span>
                    )}
                  </td>
                  <td className="muted small">{m.worktree_path}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {header}
      <div className="modal-footer-inline">
        <button type="button" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="primary"
          disabled={!canSpawn}
          onClick={submit}
        >
          Spawn
        </button>
      </div>
    </>
  );
}
