import { useEffect, useMemo, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import { CLAUDE_MODELS } from "../constants";
import { randomWorktreeBranchName } from "../utils/randomName";
import type {
  DaemonMessage,
  MemberSpawnPreview,
  PermissionMode,
  RepoEntry,
  WorkspaceEntry,
} from "../types";
import type { SpawnInitialTarget } from "./Sidebar";

interface Props {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  initialTarget?: SpawnInitialTarget | undefined;
  onClose: () => void;
  onSpawned: () => void;
}

type Mode = "single" | "workspace";
type RunMode = "interactive" | "headless" | "plain_shell";

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
  runMode: RunMode,
): {
  model: string | null;
  permission_mode: PermissionMode | null;
  extra_env: Array<[string, string]>;
} {
  const isPlainShell = runMode === "plain_shell";
  return {
    // Plain shell ignores claude-only fields; the daemon fail-fast checks
    // that they are unset, so force them to null on the wire even if the
    // user left stale state from a previous run mode.
    model: isPlainShell ? null : cfg.model,
    permission_mode: isPlainShell || skipPerms ? null : cfg.permissionMode,
    extra_env: cfg.envRows
      .filter((r) => r.key.trim().length > 0)
      .map<[string, string]>((r) => [r.key.trim(), r.value]),
  };
}

function pickInitialMode(
  initial: SpawnInitialTarget | undefined,
  workspaces: WorkspaceEntry[],
): Mode {
  if (initial?.kind === "repo") return "single";
  if (initial?.kind === "workspace") return "workspace";
  return workspaces.length > 0 ? "workspace" : "single";
}

export default function SpawnDialog({
  repos,
  workspaces,
  client,
  initialTarget,
  onClose,
  onSpawned,
}: Props) {
  const [mode, setMode] = useState<Mode>(() =>
    pickInitialMode(initialTarget, workspaces),
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

  const initialRepoId =
    initialTarget?.kind === "repo" ? initialTarget.repo_id : null;
  const initialWorkspaceId =
    initialTarget?.kind === "workspace" ? initialTarget.workspace_id : null;

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
              initialRepoId={initialRepoId}
              onClose={onClose}
              onSpawned={onSpawned}
              header={sharedFooter}
            />
          ) : (
            <WorkspaceForm
              repos={repos}
              workspaces={workspaces}
              client={client}
              skipPerms={skipPerms}
              runMode={runMode}
              headlessPrompt={headlessPrompt}
              advanced={advanced}
              initialWorkspaceId={initialWorkspaceId}
              onClose={onClose}
              onSpawned={onSpawned}
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
  const isPlainShell = runMode === "plain_shell";
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
        <label className="radio">
          <input
            type="radio"
            checked={runMode === "plain_shell"}
            onChange={() => onRunModeChange("plain_shell")}
          />
          Plain shell (no claude)
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
      {!isPlainShell && (
        <label className="checkbox">
          <input
            type="checkbox"
            checked={skipPerms}
            onChange={(e) => onSkipPermsChange(e.target.checked)}
          />
          <span>Pass --dangerously-skip-permissions</span>
        </label>
      )}
      <AdvancedSection
        skipPerms={skipPerms}
        advanced={advanced}
        onAdvancedChange={onAdvancedChange}
        isPlainShell={isPlainShell}
      />
    </>
  );
}

function AdvancedSection({
  skipPerms,
  advanced,
  onAdvancedChange,
  isPlainShell,
}: {
  skipPerms: boolean;
  advanced: AdvancedConfig;
  onAdvancedChange: (cfg: AdvancedConfig) => void;
  isPlainShell: boolean;
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
      {!isPlainShell && (
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
      )}

      {!isPlainShell && (
        <label className="field">
          <span>Permission mode</span>
          <select
            value={advanced.permissionMode ?? ""}
            onChange={(e) =>
              setPermissionMode(
                e.target.value === ""
                  ? null
                  : (e.target.value as PermissionMode),
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
      )}

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

/// Branch field shared by both forms. Behavior:
///   * On mount / when `defaultBranch` changes (entity switch): if
///     `useWorktree`, seed with a random `wt/<adj>-<noun>` (because
///     worktree-adding the default branch fails — it's already checked out
///     in the primary worktree); otherwise seed with `defaultBranch`.
///   * When `useWorktree` flips true while value still equals the default:
///     swap in a random name.
///   * When `useWorktree` flips false while value is the auto-applied
///     random one (i.e. user hasn't touched it): revert to `defaultBranch`.
///   * Any manual edit disables the auto rule until the next entity switch.
function useBranchField(defaultBranch: string, useWorktree: boolean) {
  const [value, setValue] = useState<string>(defaultBranch);
  // Track whether the random rule should still fire. Reset to true whenever
  // the entity (and therefore `defaultBranch`) changes; flipped to false the
  // moment the user edits the field by hand.
  const allowAutoRef = useRef(true);

  useEffect(() => {
    allowAutoRef.current = true;
    if (useWorktree && defaultBranch) {
      setValue(randomWorktreeBranchName());
    } else {
      setValue(defaultBranch);
    }
    // The use_worktree effect below picks up the rest after a toggle.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [defaultBranch]);

  useEffect(() => {
    if (!allowAutoRef.current) return;
    if (useWorktree) {
      // Toggling worktree on while still on the default → suggest a random.
      if (value === defaultBranch && defaultBranch) {
        setValue(randomWorktreeBranchName());
      }
    } else {
      // Toggling worktree off while still on our auto-suggested random →
      // revert to the default so the field doesn't keep an irrelevant name.
      if (value.startsWith("wt/") && defaultBranch) {
        setValue(defaultBranch);
      }
    }
    // We deliberately ignore `value` from deps — this effect is meant to
    // fire on toggle, not on every keystroke.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [useWorktree]);

  const onChange = (next: string) => {
    allowAutoRef.current = false;
    setValue(next);
  };

  return { value, setValue: onChange };
}

// ---------- single-repo form ----------

function SingleForm({
  repos,
  client,
  skipPerms,
  runMode,
  headlessPrompt,
  advanced,
  initialRepoId,
  onClose,
  onSpawned,
  header,
}: {
  repos: RepoEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  runMode: RunMode;
  headlessPrompt: string;
  advanced: AdvancedConfig;
  initialRepoId: string | null;
  onClose: () => void;
  onSpawned: () => void;
  header: React.ReactNode;
}) {
  const defaultRepoId = initialRepoId ?? repos[0]?.id ?? "";
  const [repoId, setRepoId] = useState<string>(defaultRepoId);

  const repo = useMemo(
    () => repos.find((r) => r.id === repoId) ?? null,
    [repos, repoId],
  );

  const [knownBranches, setKnownBranches] = useState<string[]>([]);
  useEffect(() => {
    if (!repoId) return;
    setKnownBranches([]);
    client.send({ type: "list_branches", repo_id: repoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "branches" || detail.repo_id !== repoId) return;
      setKnownBranches(detail.branches);
    };
    window.addEventListener("rt:branches", handler);
    return () => window.removeEventListener("rt:branches", handler);
  }, [repoId, client]);

  const defaultBranch = repo?.default_branch ?? "main";
  const [useWorktree, setUseWorktree] = useState<boolean>(true);

  // Reset the persistent-preference seed when the chosen repo changes.
  useEffect(() => {
    setUseWorktree(repo?.default_use_worktree ?? true);
  }, [repo]);

  const branch = useBranchField(defaultBranch, useWorktree);

  const [baseBranch, setBaseBranch] = useState<string>("");
  useEffect(() => {
    setBaseBranch(defaultBranch);
  }, [defaultBranch]);

  const toggleUseWorktree = (next: boolean) => {
    setUseWorktree(next);
    if (repo) {
      client.send({
        type: "set_repo_worktree_default",
        repo_id: repo.id,
        value: next,
      });
    }
  };

  const canSubmit =
    !!repoId &&
    branch.value.trim().length > 0 &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true);

  const submit = () => {
    if (!canSubmit) return;
    client.send({
      type: "spawn_session",
      label: null,
      target: {
        kind: "single",
        repo_id: repoId,
        branch_name: branch.value.trim(),
        base_branch: baseBranch.trim() || null,
        use_worktree: useWorktree,
      },
      mode: runMode,
      initial_prompt: runMode === "headless" ? headlessPrompt.trim() : null,
      dangerously_skip_permissions:
        runMode === "plain_shell" ? false : skipPerms,
      ...advancedToWire(advanced, skipPerms, runMode),
    });
    onSpawned();
    onClose();
  };

  const datalistId = `single-branches-${repoId || "none"}`;

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

      <label className="field">
        <span>Branch name</span>
        <input
          type="text"
          list={datalistId}
          value={branch.value}
          onChange={(e) => branch.setValue(e.target.value)}
          placeholder={defaultBranch}
        />
        <datalist id={datalistId}>
          {knownBranches.map((b) => (
            <option key={b} value={b} />
          ))}
        </datalist>
      </label>

      <label className="field">
        <span>Base when creating new (optional)</span>
        <input
          type="text"
          value={baseBranch}
          onChange={(e) => setBaseBranch(e.target.value)}
          placeholder={defaultBranch}
        />
      </label>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={useWorktree}
          onChange={(e) => toggleUseWorktree(e.target.checked)}
        />
        <span>
          Create a worktree
          <span className="muted small" style={{ marginLeft: 6 }}>
            (unchecked: run claude in the repo's main directory)
          </span>
        </span>
      </label>

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
  repos,
  workspaces,
  client,
  skipPerms,
  runMode,
  headlessPrompt,
  advanced,
  initialWorkspaceId,
  onClose,
  onSpawned,
  header,
}: {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  runMode: RunMode;
  headlessPrompt: string;
  advanced: AdvancedConfig;
  initialWorkspaceId: string | null;
  onClose: () => void;
  onSpawned: () => void;
  header: React.ReactNode;
}) {
  const defaultWorkspaceId = initialWorkspaceId ?? workspaces[0]?.id ?? "";
  const [workspaceId, setWorkspaceId] = useState<string>(defaultWorkspaceId);

  const workspace = useMemo(
    () => workspaces.find((w) => w.id === workspaceId) ?? null,
    [workspaces, workspaceId],
  );

  // Default branch + default_use_worktree are sourced from the first
  // registered member of the workspace (this is also what the daemon does
  // when filling in an unspecified base).
  const firstMember = useMemo(() => {
    if (!workspace) return null;
    for (const id of workspace.member_repo_ids) {
      const r = repos.find((rr) => rr.id === id);
      if (r) return r;
    }
    return null;
  }, [workspace, repos]);

  const defaultBranch = firstMember?.default_branch ?? "main";
  const [useWorktree, setUseWorktree] = useState<boolean>(true);

  useEffect(() => {
    setUseWorktree(workspace?.default_use_worktree ?? true);
  }, [workspace]);

  const branch = useBranchField(defaultBranch, useWorktree);

  const [baseBranch, setBaseBranch] = useState<string>("");
  useEffect(() => {
    setBaseBranch(defaultBranch);
  }, [defaultBranch]);

  const [preview, setPreview] = useState<MemberSpawnPreview[] | null>(null);
  useEffect(() => {
    setPreview(null);
  }, [workspaceId, branch.value, baseBranch]);

  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "workspace_spawn_preview" ||
        detail.workspace_id !== workspaceId ||
        detail.branch_name !== branch.value
      ) {
        return;
      }
      setPreview(detail.per_member);
    };
    window.addEventListener("rt:workspace_spawn_preview", handler);
    return () =>
      window.removeEventListener("rt:workspace_spawn_preview", handler);
  }, [workspaceId, branch.value]);

  const toggleUseWorktree = (next: boolean) => {
    setUseWorktree(next);
    if (workspace) {
      client.send({
        type: "set_workspace_worktree_default",
        workspace_id: workspace.id,
        value: next,
      });
    }
  };

  const requestPreview = () => {
    if (!workspaceId || !branch.value) return;
    client.send({
      type: "preview_workspace_spawn",
      workspace_id: workspaceId,
      branch_name: branch.value,
      base_branch: baseBranch.trim() || null,
    });
  };

  const canSpawn =
    !!workspaceId &&
    branch.value.trim().length > 0 &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true);

  const submit = () => {
    if (!canSpawn) return;
    client.send({
      type: "spawn_session",
      label: null,
      target: {
        kind: "workspace",
        workspace_id: workspaceId,
        branch_name: branch.value.trim(),
        base_branch: baseBranch.trim() || null,
        use_worktree: useWorktree,
      },
      mode: runMode,
      initial_prompt: runMode === "headless" ? headlessPrompt.trim() : null,
      dangerously_skip_permissions:
        runMode === "plain_shell" ? false : skipPerms,
      ...advancedToWire(advanced, skipPerms, runMode),
    });
    onSpawned();
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
          value={branch.value}
          onChange={(e) => branch.setValue(e.target.value)}
          placeholder={defaultBranch}
        />
      </label>

      <label className="field">
        <span>Base when creating new (optional)</span>
        <input
          type="text"
          value={baseBranch}
          onChange={(e) => setBaseBranch(e.target.value)}
          placeholder={defaultBranch}
        />
      </label>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={useWorktree}
          onChange={(e) => toggleUseWorktree(e.target.checked)}
        />
        <span>
          Create worktrees
          <span className="muted small" style={{ marginLeft: 6 }}>
            (unchecked: check out the branch in each member's main directory)
          </span>
        </span>
      </label>

      <div className="modal-footer-inline">
        <button
          type="button"
          onClick={requestPreview}
          disabled={!branch.value}
        >
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
                <th>Path</th>
              </tr>
            </thead>
            <tbody>
              {preview.map((m) => (
                <tr key={m.repo_id}>
                  <td>{m.repo_name}</td>
                  <td>{branch.value}</td>
                  <td>
                    {m.branch_exists ? (
                      <span className="badge badge-ok">reuse</span>
                    ) : (
                      <span className="badge badge-warn">
                        new from {m.effective_base ?? defaultBranch}
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
