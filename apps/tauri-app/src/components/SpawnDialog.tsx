import { useEffect, useMemo, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import { CLAUDE_MODELS } from "../constants";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";
import { randomWorktreeBranchName } from "../utils/randomName";
import { loadSettings } from "../utils/settings";
import type {
  Agent,
  CodexSandbox,
  DaemonMessage,
  MemberSpawnPreview,
  PermissionMode,
  RepoEntry,
  SpawnConfig,
  WorkspaceEntry,
} from "../types";
import type { SpawnInitialTarget } from "./Sidebar";

interface Props {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  initialTarget?: SpawnInitialTarget | undefined;
  /// Full SpawnConfig used to seed the dialog when invoked as
  /// "Duplicate session → Shift-click → open dialog pre-filled". When
  /// set, hydrates agent, run mode, skip-perms, model, permission mode,
  /// codex sandbox, and extra env vars from this config — overriding
  /// the usual Settings defaults. `undefined` for normal spawns.
  spawnPrefill?: SpawnConfig | undefined;
  onClose: () => void;
  onSpawned: () => void;
  /// Closes the dialog and opens the directory picker so a fresh user
  /// can register a repo when they hit the empty-state CTA.
  onAddRepo: () => void;
}

/// Regex for a syntactically valid env var key. Matches POSIX-ish names
/// (letter or underscore, then alphanumerics/underscore). Empty strings
/// are intentionally allowed (the row is treated as "not yet started").
const ENV_KEY_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

type Mode = "single" | "workspace";
type RunMode = "interactive" | "headless" | "plain_shell";

interface EnvRow {
  key: string;
  value: string;
}

interface AdvancedConfig {
  model: string | null;
  permissionMode: PermissionMode | null;
  codexSandbox: CodexSandbox | null;
  envRows: EnvRow[];
}

function emptyAdvanced(): AdvancedConfig {
  // Read defaults from Settings so users who configured a default
  // permission_mode or codex_sandbox (iter 49 Settings modal) get them
  // pre-filled. Read at dialog-open time (vs once-at-module-load) so a
  // user who flips the setting in the modal and then re-opens spawn
  // picks up the new value without restarting the app.
  const settings = loadSettings();
  return {
    model: null,
    permissionMode: settings.spawn.default_permission_mode,
    codexSandbox: settings.spawn.default_codex_sandbox,
    envRows: [],
  };
}

/// Hydrate the advanced-section state from a duplicate's SpawnConfig.
/// Mirrors the wire-vs-form translation in advancedToWire (above) but
/// in the reverse direction — env vars come back as a row list so the
/// add/remove controls work without special-casing.
function advancedFromConfig(cfg: SpawnConfig): AdvancedConfig {
  return {
    model: cfg.model,
    permissionMode: cfg.permission_mode,
    codexSandbox: cfg.codex_sandbox,
    envRows: cfg.extra_env.map(([key, value]) => ({ key, value })),
  };
}

function advancedToWire(
  cfg: AdvancedConfig,
  skipPerms: boolean,
  runMode: RunMode,
  agent: Agent,
): {
  model: string | null;
  permission_mode: PermissionMode | null;
  codex_sandbox: CodexSandbox | null;
  extra_env: Array<[string, string]>;
  prompt_injector: null;
} {
  const isPlainShell = runMode === "plain_shell";
  const isCodex = agent === "codex";
  return {
    // Plain shell ignores claude/codex-only fields; the daemon fail-fast checks
    // that they are unset, so force them to null on the wire even if the
    // user left stale state from a previous run mode.
    model: isPlainShell ? null : cfg.model,
    // Permission mode is claude-only. Skip-perms (yolo) also wins over it.
    permission_mode:
      isPlainShell || skipPerms || isCodex ? null : cfg.permissionMode,
    // Codex sandbox is codex-only. Skip-perms (yolo) wins over it too.
    codex_sandbox:
      isPlainShell || skipPerms || !isCodex ? null : cfg.codexSandbox,
    extra_env: cfg.envRows
      .filter((r) => r.key.trim().length > 0)
      .map<[string, string]>((r) => [r.key.trim(), r.value]),
    // Preset launches set this; manual spawns from this dialog do not.
    prompt_injector: null,
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
  spawnPrefill,
  onClose,
  onSpawned,
  onAddRepo,
}: Props) {
  const [mode, setMode] = useState<Mode>(() =>
    pickInitialMode(initialTarget, workspaces),
  );
  const [runMode, setRunMode] = useState<RunMode>(
    () => spawnPrefill?.mode ?? "interactive",
  );
  const [headlessPrompt, setHeadlessPrompt] = useState("");
  const [skipPerms, setSkipPerms] = useState(
    () =>
      spawnPrefill?.dangerously_skip_permissions ??
      loadSettings().spawn.skip_permissions_default,
  );
  const [agent, setAgent] = useState<Agent>(
    () => spawnPrefill?.agent ?? "claude",
  );
  const [advanced, setAdvanced] = useState<AdvancedConfig>(() =>
    spawnPrefill ? advancedFromConfig(spawnPrefill) : emptyAdvanced(),
  );

  // Headless mode isn't supported for codex yet — snap back to interactive
  // when the user picks codex while headless was selected.
  useEffect(() => {
    if (agent === "codex" && runMode === "headless") {
      setRunMode("interactive");
    }
  }, [agent, runMode]);

  const sharedFooter = (
    <Footer
      agent={agent}
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

  // Escape closes the dialog (still the keyboard escape hatch). Backdrop
  // click is intentionally NOT a close trigger — the dialog can hold a
  // dense form (custom branch, env vars, headless prompt), and a stray
  // click silently discarding it was the most-reported papercut.
  useEscape(onClose);
  useFocusReturn();

  return (
    <div className="modal-backdrop" data-testid="spawn-dialog">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label="Spawn session"
      >
        <header className="modal-header">
          <h2>Spawn session</h2>
          <button
            type="button"
            className="link"
            onClick={onClose}
            aria-label="Close dialog"
            data-testid="spawn-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          {repos.length === 0 ? (
            <EmptyRepoState onAddRepo={onAddRepo} onClose={onClose} />
          ) : (
            <>
          <fieldset className="field">
            <legend>Type</legend>
            <label className="radio">
              <input
                type="radio"
                checked={mode === "single"}
                onChange={() => setMode("single")}
                data-testid="spawn-mode-single"
              />
              Single repo
            </label>
            <label className="radio">
              <input
                type="radio"
                checked={mode === "workspace"}
                onChange={() => setMode("workspace")}
                disabled={workspaces.length === 0}
                data-testid="spawn-mode-workspace"
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
              agent={agent}
              onAgentChange={setAgent}
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
              agent={agent}
              onAgentChange={setAgent}
              initialWorkspaceId={initialWorkspaceId}
              onClose={onClose}
              onSpawned={onSpawned}
              header={sharedFooter}
            />
          )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function EmptyRepoState({
  onAddRepo,
  onClose,
}: {
  onAddRepo: () => void;
  onClose: () => void;
}) {
  return (
    <div className="empty-state" data-testid="spawn-empty-repos">
      <h3>No repos registered</h3>
      <p className="muted">
        Add a repo before spawning a session — sessions are scoped to a repo
        or a workspace, and rustling-tulip needs at least one to know where
        to run the agent.
      </p>
      <div className="modal-footer-inline">
        <button
          type="button"
          onClick={onClose}
          data-testid="spawn-empty-cancel"
        >
          Cancel
        </button>
        <button
          type="button"
          className="primary"
          onClick={() => {
            onClose();
            onAddRepo();
          }}
          data-testid="spawn-empty-add-repo"
        >
          + Add repo
        </button>
      </div>
    </div>
  );
}

function Footer({
  agent,
  skipPerms,
  onSkipPermsChange,
  runMode,
  onRunModeChange,
  headlessPrompt,
  onHeadlessPromptChange,
  advanced,
  onAdvancedChange,
}: {
  agent: Agent;
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
  const isCodex = agent === "codex";
  const headlessDisabledReason = isCodex
    ? "headless mode is not yet supported for codex"
    : undefined;
  const skipPermsLabel = isCodex
    ? "Pass --yolo (skip approvals + sandbox)"
    : "Pass --dangerously-skip-permissions";
  return (
    <>
      <fieldset className="field">
        <legend>Run mode</legend>
        <label className="radio">
          <input
            type="radio"
            checked={runMode === "interactive"}
            onChange={() => onRunModeChange("interactive")}
            data-testid="spawn-runmode-interactive"
          />
          Interactive
        </label>
        <label
          className={`radio${isCodex ? " radio-disabled" : ""}`}
          title={headlessDisabledReason}
        >
          <input
            type="radio"
            checked={runMode === "headless"}
            disabled={isCodex}
            onChange={() => onRunModeChange("headless")}
            data-testid="spawn-runmode-headless"
          />
          Headless (one-shot prompt, no terminal)
        </label>
        <label className="radio">
          <input
            type="radio"
            checked={runMode === "plain_shell"}
            onChange={() => onRunModeChange("plain_shell")}
            data-testid="spawn-runmode-plain_shell"
          />
          Plain shell (no agent)
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
            data-testid="spawn-headless-prompt"
          />
        </label>
      )}
      {!isPlainShell && (
        <label className="checkbox">
          <input
            type="checkbox"
            checked={skipPerms}
            onChange={(e) => onSkipPermsChange(e.target.checked)}
            data-testid="spawn-skip-perms"
          />
          <span>{skipPermsLabel}</span>
        </label>
      )}
      <AdvancedSection
        agent={agent}
        skipPerms={skipPerms}
        advanced={advanced}
        onAdvancedChange={onAdvancedChange}
        isPlainShell={isPlainShell}
      />
    </>
  );
}

function AdvancedSection({
  agent,
  skipPerms,
  advanced,
  onAdvancedChange,
  isPlainShell,
}: {
  agent: Agent;
  skipPerms: boolean;
  advanced: AdvancedConfig;
  onAdvancedChange: (cfg: AdvancedConfig) => void;
  isPlainShell: boolean;
}) {
  const setModel = (model: string | null) =>
    onAdvancedChange({ ...advanced, model });
  const setPermissionMode = (permissionMode: PermissionMode | null) =>
    onAdvancedChange({ ...advanced, permissionMode });
  const setCodexSandbox = (codexSandbox: CodexSandbox | null) =>
    onAdvancedChange({ ...advanced, codexSandbox });
  const setEnvRows = (envRows: EnvRow[]) =>
    onAdvancedChange({ ...advanced, envRows });
  const isCodex = agent === "codex";
  const skipPermsLabel = isCodex ? "yolo" : "skip-permissions";

  return (
    <details className="field advanced-config" data-testid="spawn-advanced">
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
          {isCodex && (
            <span className="muted small">
              Model list is claude-named; type a codex model id manually if
              none of these fit.
            </span>
          )}
        </label>
      )}

      {!isPlainShell && !isCodex && (
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
              Ignored while {skipPermsLabel} is on — claude will run without
              --permission-mode. The dropdown value is preserved for when you
              toggle {skipPermsLabel} off.
            </span>
          )}
        </label>
      )}

      {!isPlainShell && isCodex && (
        <label className="field">
          <span>Sandbox</span>
          <select
            value={advanced.codexSandbox ?? ""}
            onChange={(e) =>
              setCodexSandbox(
                e.target.value === ""
                  ? null
                  : (e.target.value as CodexSandbox),
              )
            }
            disabled={skipPerms}
          >
            <option value="">CLI default (read-only)</option>
            <option value="read-only">read-only</option>
            <option value="workspace-write">workspace-write</option>
            <option value="danger-full-access">danger-full-access</option>
          </select>
          {skipPerms && (
            <span className="muted small">
              Ignored while {skipPermsLabel} is on — codex will run with
              --yolo overriding the sandbox. The dropdown value is preserved
              for when you toggle {skipPermsLabel} off.
            </span>
          )}
        </label>
      )}

      <fieldset className="field">
        <legend>Extra environment variables</legend>
        {advanced.envRows.length === 0 && (
          <div className="muted small">No extra env vars.</div>
        )}
        {advanced.envRows.map((row, idx) => {
          const trimmed = row.key.trim();
          const invalid = trimmed.length > 0 && !ENV_KEY_RE.test(trimmed);
          const duplicate =
            trimmed.length > 0 &&
            advanced.envRows.some(
              (other, otherIdx) =>
                otherIdx !== idx && other.key.trim() === trimmed,
            );
          return (
            <div key={idx} className="env-row">
              <div className="env-row-inputs">
                <input
                  type="text"
                  placeholder="KEY"
                  value={row.key}
                  onChange={(e) => {
                    const next = advanced.envRows.slice();
                    next[idx] = { ...row, key: e.target.value };
                    setEnvRows(next);
                  }}
                  className={invalid || duplicate ? "input-invalid" : ""}
                  aria-invalid={invalid || duplicate}
                  data-testid={`spawn-env-key-${idx}`}
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
                  data-testid={`spawn-env-value-${idx}`}
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
              {invalid && (
                <div className="muted small env-row-error">
                  Key must match <code>[A-Za-z_][A-Za-z0-9_]*</code>
                </div>
              )}
              {!invalid && duplicate && (
                <div className="muted small env-row-error">
                  Duplicate key — only the last row wins
                </div>
              )}
            </div>
          );
        })}
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

/// Submit-gating helper shared by both forms. Returns true if every env
/// row's key is either empty (unstarted row) or a syntactically valid name
/// AND no two non-empty keys collide.
function envRowsAreValid(rows: EnvRow[]): boolean {
  const seen = new Set<string>();
  for (const row of rows) {
    const trimmed = row.key.trim();
    if (trimmed.length === 0) continue;
    if (!ENV_KEY_RE.test(trimmed)) return false;
    if (seen.has(trimmed)) return false;
    seen.add(trimmed);
  }
  return true;
}

function AgentPicker({
  agent,
  onChange,
}: {
  agent: Agent;
  onChange: (a: Agent) => void;
}) {
  return (
    <fieldset className="field">
      <legend>Agent</legend>
      <label className="radio">
        <input
          type="radio"
          checked={agent === "claude"}
          onChange={() => onChange("claude")}
          data-testid="spawn-agent-claude"
        />
        claude
      </label>
      <label className="radio">
        <input
          type="radio"
          checked={agent === "codex"}
          onChange={() => onChange("codex")}
          data-testid="spawn-agent-codex"
        />
        codex
      </label>
    </fieldset>
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
///
/// `targetKey` keys the suggestion across dialog opens — if the user
/// cancels and reopens with the same target, the suggested random name
/// is preserved (audit: previously regenerated on every reopen).
const branchSuggestionCache = new Map<string, string>();

function useBranchField(
  defaultBranch: string,
  useWorktree: boolean,
  targetKey: string,
) {
  const initial = useWorktree
    ? (branchSuggestionCache.get(targetKey) ?? randomWorktreeBranchName())
    : defaultBranch;
  if (useWorktree && !branchSuggestionCache.has(targetKey)) {
    branchSuggestionCache.set(targetKey, initial);
  }
  const [value, setValue] = useState<string>(initial);
  // Track whether the random rule should still fire. Reset to true whenever
  // the entity (and therefore `defaultBranch`) changes; flipped to false the
  // moment the user edits the field by hand.
  const allowAutoRef = useRef(true);

  useEffect(() => {
    allowAutoRef.current = true;
    if (useWorktree && defaultBranch) {
      const cached = branchSuggestionCache.get(targetKey);
      const next = cached ?? randomWorktreeBranchName();
      if (!cached) branchSuggestionCache.set(targetKey, next);
      setValue(next);
    } else {
      setValue(defaultBranch);
    }
    // The use_worktree effect below picks up the rest after a toggle.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [defaultBranch, targetKey]);

  useEffect(() => {
    if (!allowAutoRef.current) return;
    if (useWorktree) {
      // Toggling worktree on while still on the default → suggest a random.
      if (value === defaultBranch && defaultBranch) {
        const cached = branchSuggestionCache.get(targetKey);
        const next = cached ?? randomWorktreeBranchName();
        if (!cached) branchSuggestionCache.set(targetKey, next);
        setValue(next);
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
    // User-edited the field — drop the cached suggestion so a later
    // reopen suggests a fresh one rather than re-pinning the value
    // they just walked away from.
    branchSuggestionCache.delete(targetKey);
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
  agent,
  onAgentChange,
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
  agent: Agent;
  onAgentChange: (a: Agent) => void;
  initialRepoId: string | null;
  onClose: () => void;
  onSpawned: () => void;
  header: React.ReactNode;
}) {
  const defaultRepoId = initialRepoId ?? repos[0]?.id ?? "";
  const [repoId, setRepoId] = useState<string>(defaultRepoId);
  // Autofocus the branch name input — it's the field most likely to be
  // edited, and the random worktree name is preselected so a keyboard
  // user can just type to overwrite.
  const branchInputRef = useRef<HTMLInputElement | null>(null);
  useAutoFocus(branchInputRef);

  const repo = useMemo(
    () => repos.find((r) => r.id === repoId) ?? null,
    [repos, repoId],
  );

  // Default the agent picker to whatever this repo was last launched with.
  useEffect(() => {
    if (repo) {
      onAgentChange(repo.last_agent ?? "claude");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repo?.id]);

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

  const branch = useBranchField(defaultBranch, useWorktree, `repo:${repoId}`);

  const [baseBranch, setBaseBranch] = useState<string>("");
  useEffect(() => {
    setBaseBranch(defaultBranch);
  }, [defaultBranch]);

  // Local-only until submit. Previously this fired
  // `set_repo_worktree_default` immediately on toggle, which left the
  // persisted preference changed even if the user cancelled the spawn
  // (audit: "Worktree-default toggle persists immediately on click").
  // Submit re-sends the daemon update; cancel does nothing.
  const toggleUseWorktree = (next: boolean) => {
    setUseWorktree(next);
  };

  // Guard against double-click submit. submit() is synchronous and onClose
  // is setState; a fast double-click on the primary button could otherwise
  // fire two spawn_session messages before React unmounts the dialog.
  const submittedRef = useRef(false);

  const canSubmit =
    !!repoId &&
    branch.value.trim().length > 0 &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true) &&
    envRowsAreValid(advanced.envRows);

  const submit = () => {
    if (!canSubmit || submittedRef.current) return;
    submittedRef.current = true;
    // Successful submit consumes the cached branch suggestion so the
    // next dialog open generates a fresh one rather than re-pinning the
    // name the user just spawned with.
    branchSuggestionCache.delete(`repo:${repoId}`);
    // Persist the worktree-toggle choice as the new default for this
    // repo only on successful submit. Was previously fired live on
    // toggle, but that left the preference changed even if the user
    // cancelled (audit finding closed in iter 42).
    if (repo && useWorktree !== (repo.default_use_worktree ?? true)) {
      client.send({
        type: "set_repo_worktree_default",
        repo_id: repo.id,
        value: useWorktree,
      });
    }
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
      agent,
      ...advancedToWire(advanced, skipPerms, runMode, agent),
    });
    onSpawned();
    onClose();
  };

  const datalistId = `single-branches-${repoId || "none"}`;

  return (
    <>
      <AgentPicker agent={agent} onChange={onAgentChange} />
      <label className="field">
        <span>Repo</span>
        <select
          value={repoId}
          onChange={(e) => setRepoId(e.target.value)}
          data-testid="spawn-single-repo"
        >
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
          ref={branchInputRef}
          type="text"
          list={datalistId}
          value={branch.value}
          onChange={(e) => branch.setValue(e.target.value)}
          placeholder={defaultBranch}
          data-testid="spawn-single-branch"
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
          data-testid="spawn-single-base-branch"
        />
      </label>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={useWorktree}
          onChange={(e) => toggleUseWorktree(e.target.checked)}
          data-testid="spawn-single-worktree"
        />
        <span>
          Create a worktree
          <span className="muted small inline-note">
            (unchecked: run claude in the repo's main directory)
          </span>
        </span>
      </label>

      {header}
      <div className="modal-footer-inline">
        <button type="button" onClick={onClose} data-testid="spawn-single-cancel">
          Cancel
        </button>
        <button
          type="button"
          className="primary"
          disabled={!canSubmit}
          onClick={submit}
          data-testid="spawn-single-submit"
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
  agent,
  onAgentChange,
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
  agent: Agent;
  onAgentChange: (a: Agent) => void;
  initialWorkspaceId: string | null;
  onClose: () => void;
  onSpawned: () => void;
  header: React.ReactNode;
}) {
  const defaultWorkspaceId = initialWorkspaceId ?? workspaces[0]?.id ?? "";
  const [workspaceId, setWorkspaceId] = useState<string>(defaultWorkspaceId);
  // Autofocus the branch input — same rationale as SingleForm: it's the
  // field the user most likely wants to edit; the random worktree name
  // is preselected for easy overwrite.
  const branchInputRef = useRef<HTMLInputElement | null>(null);
  useAutoFocus(branchInputRef);

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

  // Default the agent picker to the first member repo's last_agent.
  useEffect(() => {
    if (firstMember) {
      onAgentChange(firstMember.last_agent ?? "claude");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [firstMember?.id]);

  const defaultBranch = firstMember?.default_branch ?? "main";
  const [useWorktree, setUseWorktree] = useState<boolean>(true);

  useEffect(() => {
    setUseWorktree(workspace?.default_use_worktree ?? true);
  }, [workspace]);

  const branch = useBranchField(
    defaultBranch,
    useWorktree,
    `workspace:${workspaceId}`,
  );

  const [baseBranch, setBaseBranch] = useState<string>("");
  useEffect(() => {
    setBaseBranch(defaultBranch);
  }, [defaultBranch]);

  const [preview, setPreview] = useState<MemberSpawnPreview[] | null>(null);
  useEffect(() => {
    // Clear the preview whenever any input that changes the worktree-add
    // plan changes — that includes `useWorktree`, which alters paths. Audit
    // finding: previously the preview went stale when the user toggled
    // worktree mode after a Preview, leaving the table showing paths that
    // no longer matched the active mode.
    setPreview(null);
  }, [workspaceId, branch.value, baseBranch, useWorktree]);

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

  // Local-only until submit; see SingleForm's note for rationale.
  const toggleUseWorktree = (next: boolean) => {
    setUseWorktree(next);
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

  // Mirror SingleForm's double-click guard.
  const submittedRef = useRef(false);

  const canSpawn =
    !!workspaceId &&
    branch.value.trim().length > 0 &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true) &&
    envRowsAreValid(advanced.envRows);

  const submit = () => {
    if (!canSpawn || submittedRef.current) return;
    submittedRef.current = true;
    branchSuggestionCache.delete(`workspace:${workspaceId}`);
    if (
      workspace &&
      useWorktree !== (workspace.default_use_worktree ?? true)
    ) {
      client.send({
        type: "set_workspace_worktree_default",
        workspace_id: workspace.id,
        value: useWorktree,
      });
    }
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
      agent,
      ...advancedToWire(advanced, skipPerms, runMode, agent),
    });
    onSpawned();
    onClose();
  };

  return (
    <>
      <AgentPicker agent={agent} onChange={onAgentChange} />
      <label className="field">
        <span>Workspace</span>
        <select
          value={workspaceId}
          onChange={(e) => setWorkspaceId(e.target.value)}
          data-testid="spawn-workspace-select"
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
          ref={branchInputRef}
          type="text"
          value={branch.value}
          onChange={(e) => branch.setValue(e.target.value)}
          placeholder={defaultBranch}
          data-testid="spawn-workspace-branch"
        />
      </label>

      <label className="field">
        <span>Base when creating new (optional)</span>
        <input
          type="text"
          value={baseBranch}
          onChange={(e) => setBaseBranch(e.target.value)}
          placeholder={defaultBranch}
          data-testid="spawn-workspace-base-branch"
        />
      </label>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={useWorktree}
          onChange={(e) => toggleUseWorktree(e.target.checked)}
          data-testid="spawn-workspace-worktree"
        />
        <span>
          Create worktrees
          <span className="muted small inline-note">
            (unchecked: check out the branch in each member's main directory)
          </span>
        </span>
      </label>

      <div className="modal-footer-inline">
        <button
          type="button"
          onClick={requestPreview}
          disabled={!branch.value}
          data-testid="spawn-workspace-preview"
        >
          Preview
        </button>
      </div>

      {preview && (
        <div className="preview-table" data-testid="spawn-workspace-preview-table">
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
        <button type="button" onClick={onClose} data-testid="spawn-workspace-cancel">
          Cancel
        </button>
        <button
          type="button"
          className="primary"
          disabled={!canSpawn}
          onClick={submit}
          data-testid="spawn-workspace-submit"
        >
          Spawn
        </button>
      </div>
    </>
  );
}
