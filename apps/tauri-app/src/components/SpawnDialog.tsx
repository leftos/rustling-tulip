import { useEffect, useState } from "react";
import type { DaemonClient } from "../api";
import { CLAUDE_MODELS } from "../constants";
import type {
  DaemonMessage,
  MemberSpawnPreview,
  PermissionMode,
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
type RunMode = "interactive" | "headless";

interface EnvRow {
  key: string;
  value: string;
}

interface AdvancedConfig {
  model: string | null;
  permissionMode: PermissionMode | null;
  envRows: EnvRow[];
}

function emptyAdvanced(): AdvancedConfig {
  return { model: null, permissionMode: null, envRows: [] };
}

function advancedToWire(
  cfg: AdvancedConfig,
  skipPerms: boolean,
): {
  model: string | null;
  permission_mode: PermissionMode | null;
  extra_env: Array<[string, string]>;
} {
  return {
    model: cfg.model,
    // Daemon ignores permission_mode when skip-permissions is true; mirror
    // that here so the wire payload reflects what will actually run.
    permission_mode: skipPerms ? null : cfg.permissionMode,
    extra_env: cfg.envRows
      .filter((r) => r.key.trim().length > 0)
      .map<[string, string]>((r) => [r.key.trim(), r.value]),
  };
}

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
  const [runMode, setRunMode] = useState<RunMode>("interactive");
  const [headlessPrompt, setHeadlessPrompt] = useState("");
  const [skipPerms, setSkipPerms] = useState(true);
  const [advanced, setAdvanced] = useState<AdvancedConfig>(emptyAdvanced);

  const sharedFooter = (
    <Footer
      skipPerms={skipPerms}
      onSkipPermsChange={setSkipPerms}
      runMode={runMode}
      onRunModeChange={setRunMode}
      headlessPrompt={headlessPrompt}
      onHeadlessPromptChange={setHeadlessPrompt}
      advanced={advanced}
      onAdvancedChange={setAdvanced}
    />
  );

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
              runMode={runMode}
              headlessPrompt={headlessPrompt}
              advanced={advanced}
              onClose={onClose}
              header={sharedFooter}
            />
          ) : (
            <WorkspaceForm
              workspaces={workspaces}
              client={client}
              skipPerms={skipPerms}
              runMode={runMode}
              headlessPrompt={headlessPrompt}
              advanced={advanced}
              onClose={onClose}
              header={sharedFooter}
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
  runMode,
  onRunModeChange,
  headlessPrompt,
  onHeadlessPromptChange,
  advanced,
  onAdvancedChange,
}: {
  skipPerms: boolean;
  onSkipPermsChange: (v: boolean) => void;
  runMode: RunMode;
  onRunModeChange: (m: RunMode) => void;
  headlessPrompt: string;
  onHeadlessPromptChange: (s: string) => void;
  advanced: AdvancedConfig;
  onAdvancedChange: (cfg: AdvancedConfig) => void;
}) {
  return (
    <>
      <fieldset className="field">
        <legend>Run mode</legend>
        <label className="radio">
          <input
            type="radio"
            checked={runMode === "interactive"}
            onChange={() => onRunModeChange("interactive")}
          />
          Interactive
        </label>
        <label className="radio">
          <input
            type="radio"
            checked={runMode === "headless"}
            onChange={() => onRunModeChange("headless")}
          />
          Headless (one-shot prompt, no terminal)
        </label>
      </fieldset>
      {runMode === "headless" && (
        <label className="field">
          <span>Prompt</span>
          <textarea
            rows={3}
            value={headlessPrompt}
            onChange={(e) => onHeadlessPromptChange(e.target.value)}
            placeholder="What should claude do?"
          />
        </label>
      )}
      <label className="checkbox">
        <input
          type="checkbox"
          checked={skipPerms}
          onChange={(e) => onSkipPermsChange(e.target.checked)}
        />
        <span>Pass --dangerously-skip-permissions</span>
      </label>
      <AdvancedSection
        skipPerms={skipPerms}
        advanced={advanced}
        onAdvancedChange={onAdvancedChange}
      />
    </>
  );
}

function AdvancedSection({
  skipPerms,
  advanced,
  onAdvancedChange,
}: {
  skipPerms: boolean;
  advanced: AdvancedConfig;
  onAdvancedChange: (cfg: AdvancedConfig) => void;
}) {
  const setModel = (model: string | null) =>
    onAdvancedChange({ ...advanced, model });
  const setPermissionMode = (permissionMode: PermissionMode | null) =>
    onAdvancedChange({ ...advanced, permissionMode });
  const setEnvRows = (envRows: EnvRow[]) =>
    onAdvancedChange({ ...advanced, envRows });

  return (
    <details className="field advanced-config">
      <summary>Advanced</summary>
      <label className="field">
        <span>Model</span>
        <select
          value={advanced.model ?? ""}
          onChange={(e) => setModel(e.target.value || null)}
        >
          <option value="">Use CLI default</option>
          {CLAUDE_MODELS.map((m) => (
            <option key={m.id} value={m.id}>
              {m.label}
            </option>
          ))}
        </select>
      </label>

      <label className="field">
        <span>Permission mode</span>
        <select
          value={advanced.permissionMode ?? ""}
          onChange={(e) =>
            setPermissionMode(
              e.target.value === "" ? null : (e.target.value as PermissionMode),
            )
          }
          disabled={skipPerms}
        >
          <option value="">CLI default</option>
          <option value="default">default</option>
          <option value="accept_edits">acceptEdits</option>
          <option value="bypass_permissions">bypassPermissions</option>
          <option value="plan">plan</option>
        </select>
        {skipPerms && (
          <span className="muted small">
            Disabled because skip-permissions is on
          </span>
        )}
      </label>

      <fieldset className="field">
        <legend>Extra environment variables</legend>
        {advanced.envRows.length === 0 && (
          <div className="muted small">No extra env vars.</div>
        )}
        {advanced.envRows.map((row, idx) => (
          <div
            key={idx}
            className="env-row"
            style={{ display: "flex", gap: "0.5rem", marginBottom: "0.25rem" }}
          >
            <input
              type="text"
              placeholder="KEY"
              value={row.key}
              onChange={(e) => {
                const next = advanced.envRows.slice();
                next[idx] = { ...row, key: e.target.value };
                setEnvRows(next);
              }}
              style={{ flex: "1 1 35%" }}
            />
            <input
              type="text"
              placeholder="value"
              value={row.value}
              onChange={(e) => {
                const next = advanced.envRows.slice();
                next[idx] = { ...row, value: e.target.value };
                setEnvRows(next);
              }}
              style={{ flex: "1 1 55%" }}
            />
            <button
              type="button"
              className="link"
              onClick={() =>
                setEnvRows(advanced.envRows.filter((_, i) => i !== idx))
              }
              aria-label="Remove env var"
            >
              ✕
            </button>
          </div>
        ))}
        <button
          type="button"
          onClick={() =>
            setEnvRows([...advanced.envRows, { key: "", value: "" }])
          }
        >
          + Add env var
        </button>
      </fieldset>
    </details>
  );
}

// ---------- single-repo form ----------

function SingleForm({
  repos,
  client,
  skipPerms,
  runMode,
  headlessPrompt,
  advanced,
  onClose,
  header,
}: {
  repos: RepoEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  runMode: RunMode;
  headlessPrompt: string;
  advanced: AdvancedConfig;
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
    !!repoId &&
    !!(branchMode === "existing" ? existingBranch : newBranchName) &&
    (branchMode === "new" ? !!baseBranch : true) &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true);

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
      mode: runMode,
      initial_prompt: runMode === "headless" ? headlessPrompt.trim() : null,
      dangerously_skip_permissions: skipPerms,
      ...advancedToWire(advanced, skipPerms),
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
  runMode,
  headlessPrompt,
  advanced,
  onClose,
  header,
}: {
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  runMode: RunMode;
  headlessPrompt: string;
  advanced: AdvancedConfig;
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
    !!preview &&
    previewBranch === branchName &&
    !!workspaceId &&
    !!branchName &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true);

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
      mode: runMode,
      initial_prompt: runMode === "headless" ? headlessPrompt.trim() : null,
      dangerously_skip_permissions: skipPerms,
      ...advancedToWire(advanced, skipPerms),
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
