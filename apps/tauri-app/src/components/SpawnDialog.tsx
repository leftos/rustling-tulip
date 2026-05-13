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
  TabEntry,
  WorkspaceEntry,
  WorktreeInfo,
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
  canUseCurrentTab: boolean;
  currentTabName: string | null;
  /// All existing tabs, surfaced as per-tab radios in the placement
  /// picker so the user can land a fresh spawn in any tab without
  /// switching focus first. Tabs that can't host panes (e.g. diff tabs)
  /// are filtered out by the caller.
  tabs: TabEntry[];
  /// Active main-window tab id; used to label "Current tab" with the
  /// active tab's name and to hide the duplicate per-tab radio entry.
  activeTabId: string | null;
  onClose: () => void;
  onSpawned: (placement: SpawnPlacement) => void;
  /// Closes the dialog and opens the directory picker so a fresh user
  /// can register a repo when they hit the empty-state CTA.
  onAddRepo: () => void;
}

/// Regex for a syntactically valid env var key. Matches POSIX-ish names
/// (letter or underscore, then alphanumerics/underscore). Empty strings
/// are intentionally allowed (the row is treated as "not yet started").
const ENV_KEY_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

type RunMode = "interactive" | "headless" | "plain_shell";

/// Identifier for whatever the user picked in the unified Target picker.
/// `kind` discriminates which form renders; `id` is the corresponding
/// repo_id or workspace_id.
type TargetSelection =
  | { kind: "repo"; id: string }
  | { kind: "workspace"; id: string };

/// Serialize a target for use as a React `key`. Remounting the form on
/// every target switch — cheap state reset, no manual sync needed.
function targetKey(t: TargetSelection): string {
  return `${t.kind}:${t.id}`;
}

/// Where the spawned session lands. `current_tab` resolves to whichever
/// tab is active at submit time (App.tsx then defers to any pane/tab
/// target the user opened the dialog from). `tab` pins placement to a
/// specific tab id — surfaced as one radio per existing tab in the
/// placement picker.
export type SpawnPlacement =
  | { kind: "current_tab" }
  | { kind: "new_tab" }
  | { kind: "tab"; tabId: string };

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

function defaultRunModeFromLastSpawn(config: SpawnConfig | null): RunMode {
  // Headless prompts are intentionally one-shot, so only plain shell becomes
  // a sticky runtime preference for normal dialog opens.
  return config?.mode === "plain_shell" ? "plain_shell" : "interactive";
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

/// Pick the initial selection for the unified Target picker. Priority:
///   1. Honor an explicit `initialTarget` (sidebar context menu).
///   2. Fall back to the first registered workspace when one exists —
///      preserves the legacy default where workspaces sort before repos
///      in the list.
///   3. Otherwise the first repo.
/// Returns `null` only when there is genuinely nothing to spawn into;
/// the caller renders the empty-state CTA in that case.
function pickInitialTarget(
  initial: SpawnInitialTarget | undefined,
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
): TargetSelection | null {
  if (initial?.kind === "repo") return { kind: "repo", id: initial.repo_id };
  if (initial?.kind === "workspace")
    return { kind: "workspace", id: initial.workspace_id };
  const firstWorkspace = workspaces[0];
  if (firstWorkspace) return { kind: "workspace", id: firstWorkspace.id };
  const firstRepo = repos[0];
  if (firstRepo) return { kind: "repo", id: firstRepo.id };
  return null;
}

export default function SpawnDialog({
  repos,
  workspaces,
  client,
  initialTarget,
  spawnPrefill,
  canUseCurrentTab,
  currentTabName,
  tabs,
  activeTabId,
  onClose,
  onSpawned,
  onAddRepo,
}: Props) {
  const [target, setTarget] = useState<TargetSelection | null>(() =>
    pickInitialTarget(initialTarget, repos, workspaces),
  );

  // If the selected target disappears mid-dialog (repo removed elsewhere),
  // snap back to the first available container. Avoids a render-cycle
  // where `target` references a non-existent entry.
  useEffect(() => {
    if (!target) return;
    const stillExists =
      target.kind === "repo"
        ? repos.some((r) => r.id === target.id)
        : workspaces.some((w) => w.id === target.id);
    if (!stillExists) {
      setTarget(pickInitialTarget(undefined, repos, workspaces));
    }
  }, [target, repos, workspaces]);

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
  const [spawnPlacement, setSpawnPlacement] = useState<SpawnPlacement>(() =>
    canUseCurrentTab ? { kind: "current_tab" } : { kind: "new_tab" },
  );
  const restoreLastSpawnDefaults = spawnPrefill === undefined;

  // Headless mode isn't supported for codex yet — snap back to interactive
  // when the user picks codex while headless was selected.
  useEffect(() => {
    if (agent === "codex" && runMode === "headless") {
      setRunMode("interactive");
    }
  }, [agent, runMode]);

  useEffect(() => {
    if (!canUseCurrentTab && spawnPlacement.kind === "current_tab") {
      setSpawnPlacement({ kind: "new_tab" });
    }
  }, [canUseCurrentTab, spawnPlacement]);

  // If the user picked a specific tab and that tab disappears (closed
  // while the dialog is open), fall back to current_tab / new_tab.
  useEffect(() => {
    if (spawnPlacement.kind !== "tab") return;
    if (tabs.some((t) => t.id === spawnPlacement.tabId)) return;
    setSpawnPlacement(
      canUseCurrentTab ? { kind: "current_tab" } : { kind: "new_tab" },
    );
  }, [tabs, spawnPlacement, canUseCurrentTab]);

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
              <AgentPicker
                agent={agent}
                runMode={runMode}
                onAgentChange={setAgent}
                onRunModeChange={setRunMode}
              />
              <SpawnPlacementPicker
                value={spawnPlacement}
                canUseCurrentTab={canUseCurrentTab}
                currentTabName={currentTabName}
                tabs={tabs}
                activeTabId={activeTabId}
                onChange={setSpawnPlacement}
              />
              <TargetPicker
                value={target}
                repos={repos}
                workspaces={workspaces}
                onChange={setTarget}
              />

              {target?.kind === "repo" ? (
                <SingleForm
                  key={targetKey(target)}
                  repoId={target.id}
                  repos={repos}
                  client={client}
                  skipPerms={skipPerms}
                  runMode={runMode}
                  headlessPrompt={headlessPrompt}
                  advanced={advanced}
                  agent={agent}
                  onAgentChange={setAgent}
                  onRunModeChange={setRunMode}
                  restoreLastSpawnDefaults={restoreLastSpawnDefaults}
                  spawnPlacement={spawnPlacement}
                  onClose={onClose}
                  onSpawned={onSpawned}
                  header={sharedFooter}
                />
              ) : target?.kind === "workspace" ? (
                <WorkspaceForm
                  key={targetKey(target)}
                  workspaceId={target.id}
                  repos={repos}
                  workspaces={workspaces}
                  client={client}
                  skipPerms={skipPerms}
                  runMode={runMode}
                  headlessPrompt={headlessPrompt}
                  advanced={advanced}
                  agent={agent}
                  onAgentChange={setAgent}
                  onRunModeChange={setRunMode}
                  restoreLastSpawnDefaults={restoreLastSpawnDefaults}
                  spawnPlacement={spawnPlacement}
                  onClose={onClose}
                  onSpawned={onSpawned}
                  header={sharedFooter}
                />
              ) : null}
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
      {!isPlainShell && (
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
        </fieldset>
      )}
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
  runMode,
  onAgentChange,
  onRunModeChange,
}: {
  agent: Agent;
  runMode: RunMode;
  onAgentChange: (a: Agent) => void;
  onRunModeChange: (m: RunMode) => void;
}) {
  const isPlainShell = runMode === "plain_shell";
  const selectAgent = (next: Agent) => {
    onAgentChange(next);
    if (isPlainShell) {
      onRunModeChange("interactive");
    }
  };

  return (
    <fieldset className="field">
      <legend>Runtime</legend>
      <label className="radio">
        <input
          type="radio"
          checked={!isPlainShell && agent === "claude"}
          onChange={() => selectAgent("claude")}
          data-testid="spawn-agent-claude"
        />
        claude
      </label>
      <label className="radio">
        <input
          type="radio"
          checked={!isPlainShell && agent === "codex"}
          onChange={() => selectAgent("codex")}
          data-testid="spawn-agent-codex"
        />
        codex
      </label>
      <label className="radio">
        <input
          type="radio"
          checked={isPlainShell}
          onChange={() => onRunModeChange("plain_shell")}
          data-testid="spawn-agent-plain-shell"
        />
        Plain shell
      </label>
    </fieldset>
  );
}

function TargetPicker({
  value,
  repos,
  workspaces,
  onChange,
}: {
  value: TargetSelection | null;
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  onChange: (next: TargetSelection) => void;
}) {
  // The picker is a single <select> listing repos and workspaces with
  // bracketed kind tags. We encode each option's value as
  // `repo:<id>` / `workspace:<id>` so the change handler can decode
  // without a second lookup.
  const currentValue = value ? targetKey(value) : "";
  const handleChange = (raw: string) => {
    const sep = raw.indexOf(":");
    if (sep === -1) return;
    const kind = raw.slice(0, sep);
    const id = raw.slice(sep + 1);
    if (kind === "repo") onChange({ kind: "repo", id });
    else if (kind === "workspace") onChange({ kind: "workspace", id });
  };
  return (
    <label className="field">
      <span>Target</span>
      <select
        value={currentValue}
        onChange={(e) => handleChange(e.target.value)}
        data-testid="spawn-target-select"
      >
        {repos.map((r) => (
          <option
            key={`repo:${r.id}`}
            value={`repo:${r.id}`}
            data-testid={`spawn-target-option-repo-${r.id}`}
          >
            [REPO]  {r.name}
          </option>
        ))}
        {workspaces.map((w) => (
          <option
            key={`workspace:${w.id}`}
            value={`workspace:${w.id}`}
            data-testid={`spawn-target-option-workspace-${w.id}`}
          >
            [WS]    {w.name} ({w.member_repo_ids.length} repos)
          </option>
        ))}
      </select>
    </label>
  );
}

function SpawnPlacementPicker({
  value,
  canUseCurrentTab,
  currentTabName,
  tabs,
  activeTabId,
  onChange,
}: {
  value: SpawnPlacement;
  canUseCurrentTab: boolean;
  currentTabName: string | null;
  tabs: TabEntry[];
  activeTabId: string | null;
  onChange: (placement: SpawnPlacement) => void;
}) {
  // The active tab is already represented by "Current tab" — hide its
  // explicit per-tab radio to avoid two identical-effect choices side by
  // side. The non-active tabs each get their own radio.
  const otherTabs = tabs.filter((t) => t.id !== activeTabId);
  return (
    <fieldset className="field">
      <legend>Open in</legend>
      <label
        className={`radio${canUseCurrentTab ? "" : " radio-disabled"}`}
        title={
          canUseCurrentTab
            ? undefined
            : "The current tab cannot host terminal panes"
        }
      >
        <input
          type="radio"
          checked={value.kind === "current_tab"}
          disabled={!canUseCurrentTab}
          onChange={() => onChange({ kind: "current_tab" })}
          data-testid="spawn-placement-current-tab"
        />
        Current tab
        {currentTabName && (
          <span className="muted small inline-note">{currentTabName}</span>
        )}
      </label>
      <label className="radio">
        <input
          type="radio"
          checked={value.kind === "new_tab"}
          onChange={() => onChange({ kind: "new_tab" })}
          data-testid="spawn-placement-new-tab"
        />
        New tab
      </label>
      {otherTabs.map((t) => (
        <label key={`placement:${t.id}`} className="radio">
          <input
            type="radio"
            checked={value.kind === "tab" && value.tabId === t.id}
            onChange={() => onChange({ kind: "tab", tabId: t.id })}
            data-testid={`spawn-placement-tab-${t.id}`}
          />
          {t.name}
        </label>
      ))}
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
  repoId,
  repos,
  client,
  skipPerms,
  runMode,
  headlessPrompt,
  advanced,
  agent,
  onAgentChange,
  onRunModeChange,
  restoreLastSpawnDefaults,
  spawnPlacement,
  onClose,
  onSpawned,
  header,
}: {
  /// Selected repo id — owned by the dialog's unified Target picker.
  /// The form remounts (via `key`) whenever this changes, so internal
  /// state resets cleanly without effects to sync.
  repoId: string;
  repos: RepoEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  runMode: RunMode;
  headlessPrompt: string;
  advanced: AdvancedConfig;
  agent: Agent;
  onAgentChange: (a: Agent) => void;
  onRunModeChange: (m: RunMode) => void;
  restoreLastSpawnDefaults: boolean;
  spawnPlacement: SpawnPlacement;
  onClose: () => void;
  onSpawned: (placement: SpawnPlacement) => void;
  header: React.ReactNode;
}) {
  // Autofocus the branch name input — it's the field most likely to be
  // edited, and the random worktree name is preselected so a keyboard
  // user can just type to overwrite.
  const branchInputRef = useRef<HTMLInputElement | null>(null);
  useAutoFocus(branchInputRef);

  const repo = useMemo(
    () => repos.find((r) => r.id === repoId) ?? null,
    [repos, repoId],
  );

  // Default runtime controls to whatever this repo was last launched with.
  useEffect(() => {
    if (repo && restoreLastSpawnDefaults) {
      onAgentChange(
        repo.last_spawn_config?.agent ?? repo.last_agent ?? "claude",
      );
      onRunModeChange(defaultRunModeFromLastSpawn(repo.last_spawn_config));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repo?.id, restoreLastSpawnDefaults]);

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
  /// "new" creates a fresh worktree (auto-named or user-typed branch).
  /// "existing" picks an already-checked-out worktree from this repo —
  /// useful for returning to a parked session's worktree, or one created
  /// by another session that has since been removed.
  const [worktreeMode, setWorktreeMode] = useState<"new" | "existing">("new");
  const [worktreeList, setWorktreeList] = useState<WorktreeInfo[]>([]);

  // Reset the persistent-preference seed when the chosen repo changes.
  useEffect(() => {
    setUseWorktree(repo?.default_use_worktree ?? true);
    // Flip back to "new" on repo change — the existing worktree list
    // belongs to the previous repo and would be misleading.
    setWorktreeMode("new");
    setWorktreeList([]);
  }, [repo]);

  // Fetch the worktree list when entering "existing" mode for the current
  // repo. Mirrors the list_branches pattern above.
  useEffect(() => {
    if (!repoId || !useWorktree || worktreeMode !== "existing") return;
    setWorktreeList([]);
    client.send({ type: "list_worktrees", repo_id: repoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "worktrees" || detail.repo_id !== repoId) return;
      setWorktreeList(detail.worktrees);
    };
    window.addEventListener("rt:worktrees", handler);
    return () => window.removeEventListener("rt:worktrees", handler);
  }, [repoId, useWorktree, worktreeMode, client]);

  const branch = useBranchField(defaultBranch, useWorktree, `repo:${repoId}`);

  const [baseBranch, setBaseBranch] = useState<string>("");
  useEffect(() => {
    setBaseBranch(defaultBranch);
  }, [defaultBranch]);

  /// Worktrees a user can still pick — entries already bound to a live
  /// session are shown disabled (greyed) below, so we keep them in the
  /// list rather than hiding (clarifies *why* a branch is unavailable).
  const availableWorktreeCount = useMemo(
    () => worktreeList.filter((w) => !w.is_active).length,
    [worktreeList],
  );

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

  /// In "existing" mode the branch must match one of the worktrees the
  /// daemon reported, AND that worktree must not be in active use. This
  /// blocks the placeholder-not-yet-picked state from submitting silently.
  const existingWorktreeOk =
    !useWorktree ||
    worktreeMode === "new" ||
    worktreeList.some(
      (w) => w.branch === branch.value.trim() && !w.is_active,
    );

  const canSubmit =
    !!repoId &&
    branch.value.trim().length > 0 &&
    existingWorktreeOk &&
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
    onSpawned(spawnPlacement);
    onClose();
  };

  const datalistId = `single-branches-${repoId || "none"}`;

  return (
    <>
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

      {useWorktree && (
        <div className="field" role="radiogroup" aria-label="Worktree mode">
          <span>Worktree</span>
          <div className="segmented">
            <button
              type="button"
              className={worktreeMode === "new" ? "active" : ""}
              onClick={() => setWorktreeMode("new")}
              data-testid="spawn-single-worktree-mode-new"
            >
              New worktree
            </button>
            <button
              type="button"
              className={worktreeMode === "existing" ? "active" : ""}
              onClick={() => setWorktreeMode("existing")}
              data-testid="spawn-single-worktree-mode-existing"
            >
              Use existing
            </button>
          </div>
        </div>
      )}

      {useWorktree && worktreeMode === "existing" ? (
        <label className="field">
          <span>Existing worktree</span>
          <select
            value={branch.value}
            onChange={(e) => branch.setValue(e.target.value)}
            data-testid="spawn-single-worktree-picker"
          >
            <option value="" disabled>
              {worktreeList.length === 0
                ? "Loading…"
                : availableWorktreeCount === 0
                  ? "(no available worktrees)"
                  : "Choose a worktree…"}
            </option>
            {worktreeList.map((w) => (
              <option
                key={w.path}
                value={w.branch}
                disabled={w.is_active}
                title={w.path}
              >
                {w.branch}
                {w.is_active ? " (in use)" : ""}
              </option>
            ))}
          </select>
        </label>
      ) : (
        <>
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
        </>
      )}

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
  workspaceId,
  repos,
  workspaces,
  client,
  skipPerms,
  runMode,
  headlessPrompt,
  advanced,
  agent,
  onAgentChange,
  onRunModeChange,
  restoreLastSpawnDefaults,
  spawnPlacement,
  onClose,
  onSpawned,
  header,
}: {
  /// Selected workspace id — owned by the dialog's unified Target picker.
  workspaceId: string;
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  skipPerms: boolean;
  runMode: RunMode;
  headlessPrompt: string;
  advanced: AdvancedConfig;
  agent: Agent;
  onAgentChange: (a: Agent) => void;
  onRunModeChange: (m: RunMode) => void;
  restoreLastSpawnDefaults: boolean;
  spawnPlacement: SpawnPlacement;
  onClose: () => void;
  onSpawned: (placement: SpawnPlacement) => void;
  header: React.ReactNode;
}) {
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

  // Default runtime controls to the workspace's last spawn when available.
  useEffect(() => {
    if (restoreLastSpawnDefaults && (workspace || firstMember)) {
      onAgentChange(
        workspace?.last_spawn_config?.agent ??
          firstMember?.last_agent ??
          "claude",
      );
      onRunModeChange(
        defaultRunModeFromLastSpawn(workspace?.last_spawn_config ?? null),
      );
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspace?.id, firstMember?.id, restoreLastSpawnDefaults]);

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
    onSpawned(spawnPlacement);
    onClose();
  };

  return (
    <>
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
