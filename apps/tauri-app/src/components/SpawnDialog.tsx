import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import { CLAUDE_MODELS } from "../constants";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";
import { useIsRemote } from "../utils/remoteMode";
import BranchCombobox from "./BranchCombobox";
import { randomWorktreeBranchName } from "../utils/randomName";
import { loadSettings } from "../utils/settings";
import type {
  Agent,
  AgentOptions,
  ClientMessage,
  CodexSandbox,
  CursorSandbox,
  DaemonMessage,
  MemberSpawnPreview,
  PermissionMode,
  PinnedMemberWorktree,
  RepoEntry,
  RootWorktreeEntry,
  RootWorktreeStatus,
  SpawnConfig,
  TabEntry,
  WorkspaceEntry,
  WorktreeInfo,
  WorktreeLaunchTarget,
  WorktreeReuseChoice,
} from "../types";
import type { SpawnInitialTarget } from "./Sidebar";

interface Props {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  initialTarget?: SpawnInitialTarget | undefined;
  /// When true, `initialTarget` locks the picker into an
  /// `ImpliedTargetLabel`. When false (or unset), `initialTarget` only
  /// seeds the picker and the user can still pick a different
  /// repo/workspace. Defaults to true to preserve the sidebar
  /// context-menu / launch-last behavior where the caller has already
  /// committed to a container. The empty-pane "spawn here" path opts
  /// out so the inferred sibling target stays editable.
  lockInitialTarget?: boolean;
  /// Full SpawnConfig used to seed the dialog when invoked as
  /// "Duplicate session → Shift-click → open dialog pre-filled". When
  /// set, hydrates agent, run mode, trusted-launch state, model, permission mode,
  /// codex sandbox, and extra env vars from this config — overriding
  /// the usual Settings defaults. `undefined` for normal spawns.
  spawnPrefill?: SpawnConfig | undefined;
  /// Worktree group this dialog was opened pinned to, from "Launch session
  /// here" in the worktrees manager. Unlike `spawnPrefill` it decides only
  /// *where* the session runs — agent, mode, model, and prompt keep their
  /// normal defaults.
  spawnPin?: WorktreeLaunchTarget | undefined;
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
  /// Hands the just-sent spawn request up so the app can resend it with a
  /// checkout strategy if the daemon asks to confirm a dirty in-place switch.
  onSpawnRequest: (req: Extract<ClientMessage, { type: "spawn_session" }>) => void;
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
  cursorSandbox: CursorSandbox | null;
  cursorPlanMode: boolean;
  envRows: EnvRow[];
}

function emptyAdvanced(): AdvancedConfig {
  // Read defaults from Settings so users who configured a default
  // permission_mode or codex_sandbox get them pre-filled. Read at
  // dialog-open time so changes apply without restarting the app.
  const settings = loadSettings();
  return {
    model: null,
    permissionMode: settings.spawn.default_permission_mode,
    codexSandbox: settings.spawn.default_codex_sandbox,
    cursorSandbox: null,
    cursorPlanMode: false,
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
/// add/remove controls work without special-casing. The agent-specific
/// `permissionMode` / `codexSandbox` slots populate from whichever variant
/// of `agent_options` is present; the other slot stays null and is
/// presented blank if the user later flips the radio.
function advancedFromConfig(cfg: SpawnConfig): AdvancedConfig {
  const opts = cfg.agent_options;
  return {
    model: cfg.model,
    permissionMode: opts.kind === "claude" ? opts.permission_mode : null,
    codexSandbox: opts.kind === "codex" ? opts.sandbox : null,
    cursorSandbox: opts.kind === "cursor" ? opts.sandbox : null,
    cursorPlanMode: opts.kind === "cursor" ? opts.plan_mode : false,
    envRows: cfg.extra_env.map(([key, value]) => ({ key, value })),
  };
}

/// Build the `AgentOptions` discriminated union the daemon expects on the
/// wire. Trusted launches and plain-shell spawns drop the agent-specific
/// inner fields so the daemon's fail-fast validation sees a clean shape.
function buildAgentOptions(
  agent: Agent,
  cfg: AdvancedConfig,
  skipPerms: boolean,
  isPlainShell: boolean,
): AgentOptions {
  const suppressInner = isPlainShell || skipPerms;
  if (agent === "claude") {
    return {
      kind: "claude",
      permission_mode: suppressInner ? null : cfg.permissionMode,
    };
  }
  if (agent === "codex") {
    return {
      kind: "codex",
      sandbox: suppressInner ? null : cfg.codexSandbox,
    };
  }
  return {
    kind: "cursor",
    plan_mode: isPlainShell ? false : cfg.cursorPlanMode,
    sandbox: suppressInner ? null : cfg.cursorSandbox,
  };
}

function advancedToWire(
  cfg: AdvancedConfig,
  runMode: RunMode,
): {
  model: string | null;
  extra_env: Array<[string, string]>;
  prompt_injector: null;
} {
  const isPlainShell = runMode === "plain_shell";
  return {
    // Plain shell ignores agent-specific fields; the daemon fail-fast checks
    // that they are unset, so force model to null on the wire even if the
    // user left stale state from a previous run mode.
    model: isPlainShell ? null : cfg.model,
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
  lockInitialTarget = true,
  spawnPrefill,
  spawnPin,
  canUseCurrentTab,
  currentTabName,
  tabs,
  activeTabId,
  onClose,
  onSpawned,
  onSpawnRequest,
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
    () => spawnPrefill?.agent_options.kind ?? "claude",
  );
  const [advanced, setAdvanced] = useState<AdvancedConfig>(() =>
    spawnPrefill ? advancedFromConfig(spawnPrefill) : emptyAdvanced(),
  );
  const [spawnPlacement, setSpawnPlacement] = useState<SpawnPlacement>(() =>
    canUseCurrentTab ? { kind: "current_tab" } : { kind: "new_tab" },
  );
  const restoreLastSpawnDefaults = spawnPrefill === undefined;

  // Track whether the user has manually changed agent / run mode. Once
  // touched, the auto-default-from-last_spawn_config effect below stops
  // overriding their choice on subsequent target changes. Refs (not
  // state) so flipping a flag doesn't itself re-trigger any effect.
  const agentTouchedRef = useRef(false);
  const runModeTouchedRef = useRef(false);
  const handleAgentChange = useCallback((next: Agent) => {
    agentTouchedRef.current = true;
    setAgent(next);
  }, []);
  const handleRunModeChange = useCallback((next: RunMode) => {
    runModeTouchedRef.current = true;
    setRunMode(next);
  }, []);

  // When the user picks a different repo/workspace, default the agent
  // and run mode from that container's last_spawn_config — unless they've
  // already explicitly set one of those fields, in which case their
  // choice wins. Replaces per-form effects that snapped these back to
  // the new target's defaults on every switch, blowing away a manual
  // shell-type change made earlier in the same dialog.
  useEffect(() => {
    if (!restoreLastSpawnDefaults) return;
    if (!target) return;
    let nextAgent: Agent | null = null;
    let nextRunMode: RunMode | null = null;
    if (target.kind === "repo") {
      const repo = repos.find((r) => r.id === target.id);
      if (!repo) return;
      nextAgent =
        repo.last_spawn_config?.agent_options.kind ??
        repo.last_agent ??
        "claude";
      nextRunMode = defaultRunModeFromLastSpawn(repo.last_spawn_config);
    } else {
      const workspace = workspaces.find((w) => w.id === target.id);
      if (!workspace) return;
      const firstMember = workspace.member_repo_ids
        .map((id) => repos.find((r) => r.id === id))
        .find((r): r is RepoEntry => !!r);
      nextAgent =
        workspace.last_spawn_config?.agent_options.kind ??
        firstMember?.last_agent ??
        "claude";
      nextRunMode = defaultRunModeFromLastSpawn(
        workspace.last_spawn_config ?? null,
      );
    }
    if (nextAgent !== null && !agentTouchedRef.current) {
      setAgent(nextAgent);
    }
    if (nextRunMode !== null && !runModeTouchedRef.current) {
      setRunMode(nextRunMode);
    }
    // `repos` / `workspaces` are not listed: the auto-default should
    // re-fire only when the user picks a different target, not when an
    // unrelated session_updated broadcast rebuilds those arrays.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [target?.kind, target?.id, restoreLastSpawnDefaults]);

  // Headless mode isn't supported for cursor yet — snap back to interactive
  // when the user picks cursor while headless was selected. Codex and claude
  // both speak headless (claude via stream-json, codex via exec --json).
  useEffect(() => {
    if (agent === "cursor" && runMode === "headless") {
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
      onRunModeChange={handleRunModeChange}
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
              {initialTarget !== undefined && lockInitialTarget && target ? (
                <ImpliedTargetLabel
                  target={target}
                  repos={repos}
                  workspaces={workspaces}
                />
              ) : (
                <TargetPicker
                  value={target}
                  repos={repos}
                  workspaces={workspaces}
                  onChange={setTarget}
                />
              )}
              <AgentPicker
                agent={agent}
                runMode={runMode}
                onAgentChange={handleAgentChange}
                onRunModeChange={handleRunModeChange}
              />
              <SpawnPlacementPicker
                value={spawnPlacement}
                canUseCurrentTab={canUseCurrentTab}
                currentTabName={currentTabName}
                tabs={tabs}
                activeTabId={activeTabId}
                onChange={setSpawnPlacement}
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
                  spawnPrefill={spawnPrefill}
                  spawnPin={spawnPin}
                  spawnPlacement={spawnPlacement}
                  onClose={onClose}
                  onSpawned={onSpawned}
                  onSpawnRequest={onSpawnRequest}
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
                  spawnPrefill={spawnPrefill}
                  spawnPin={spawnPin}
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
  const isRemote = useIsRemote();
  return (
    <div className="empty-state" data-testid="spawn-empty-repos">
      <h3>No repos registered</h3>
      <p className="muted">
        {isRemote
          ? "The host has no repos registered. Repos are managed on the host machine — add one there before spawning a session."
          : "Add a repo before spawning a session — sessions are scoped to a repo or a workspace, and rustling-tulip needs at least one to know where to run the agent."}
      </p>
      <div className="modal-footer-inline">
        <button
          type="button"
          onClick={onClose}
          data-testid="spawn-empty-cancel"
        >
          {isRemote ? "Close" : "Cancel"}
        </button>
        {!isRemote && (
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
        )}
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
  const isCursor = agent === "cursor";
  const headlessDisabledReason = isCursor
    ? "headless mode is not yet supported for cursor"
    : undefined;
  const headlessLocked = isCursor;
  const useYolo = isCodex || isCursor;
  const authorityFlag = useYolo ? "--yolo" : "--dangerously-skip-permissions";
  const authorityDetail = useYolo
    ? `${agent} approvals and sandboxing are bypassed for this session.`
    : "Claude approval prompts are bypassed for this session.";
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
            className={`radio${headlessLocked ? " radio-disabled" : ""}`}
            title={headlessDisabledReason}
          >
            <input
              type="radio"
              checked={runMode === "headless"}
              disabled={headlessLocked}
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
        <>
          <label
            className={`checkbox spawn-authority-toggle${skipPerms ? " elevated" : ""}`}
            data-testid="spawn-authority-toggle"
          >
            <input
              type="checkbox"
              checked={skipPerms}
              onChange={(e) => onSkipPermsChange(e.target.checked)}
              data-testid="spawn-skip-perms"
            />
            <span>
              Trusted launch{" "}
              <span className="muted small">({authorityFlag})</span>
            </span>
          </label>
          {skipPerms && (
            <div
              className="spawn-authority-warning"
              role="status"
              data-testid="spawn-trusted-launch-warning"
            >
              <strong>Trusted launch</strong>
              <span>
                {authorityDetail} Uses <code>{authorityFlag}</code>.
              </span>
            </div>
          )}
        </>
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
  const setCursorSandbox = (cursorSandbox: CursorSandbox | null) =>
    onAdvancedChange({ ...advanced, cursorSandbox });
  const setCursorPlanMode = (cursorPlanMode: boolean) =>
    onAdvancedChange({ ...advanced, cursorPlanMode });
  const setEnvRows = (envRows: EnvRow[]) =>
    onAdvancedChange({ ...advanced, envRows });
  const isCodex = agent === "codex";
  const isCursor = agent === "cursor";
  const trustedLaunchLabel = "trusted launch";

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
          {(isCodex || isCursor) && (
            <span className="muted small">
              Model list is claude-named; type a {agent} model id manually if
              none of these fit.
            </span>
          )}
        </label>
      )}

      {!isPlainShell && !isCodex && !isCursor && (
        <label className="field">
          <span>Claude approval mode</span>
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
            <option value="accept_edits">accept edits</option>
            <option value="bypass_permissions">bypass permissions</option>
            <option value="plan">plan</option>
          </select>
          {skipPerms && (
            <span className="muted small">
              Ignored while {trustedLaunchLabel} is on. Claude will run
              without --permission-mode. The dropdown value is preserved for
              when you toggle {trustedLaunchLabel} off.
            </span>
          )}
        </label>
      )}

      {!isPlainShell && isCodex && (
        <label className="field">
          <span>Codex sandbox mode</span>
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
              Ignored while {trustedLaunchLabel} is on. Codex will run with
              --yolo, which overrides sandbox mode. The dropdown value is
              preserved for when you toggle {trustedLaunchLabel} off.
            </span>
          )}
        </label>
      )}

      {!isPlainShell && isCursor && (
        <>
          <label
            className="checkbox"
            data-testid="spawn-cursor-plan-mode"
            title="Start cursor in --plan mode (read-only / planning)"
          >
            <input
              type="checkbox"
              checked={advanced.cursorPlanMode}
              onChange={(e) => setCursorPlanMode(e.target.checked)}
            />
            <span>Plan mode (read-only / planning)</span>
          </label>
          <label className="field">
            <span>Cursor sandbox</span>
            <select
              value={advanced.cursorSandbox ?? ""}
              onChange={(e) =>
                setCursorSandbox(
                  e.target.value === ""
                    ? null
                    : (e.target.value as CursorSandbox),
                )
              }
              disabled={skipPerms}
            >
              <option value="">CLI default</option>
              <option value="enabled">enabled</option>
              <option value="disabled">disabled</option>
            </select>
            {skipPerms && (
              <span className="muted small">
                Ignored while {trustedLaunchLabel} is on. Cursor will run with
                --yolo, which overrides sandbox mode. The dropdown value is
                preserved for when you toggle {trustedLaunchLabel} off.
              </span>
            )}
          </label>
        </>
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
          checked={!isPlainShell && agent === "cursor"}
          onChange={() => selectAgent("cursor")}
          data-testid="spawn-agent-cursor"
        />
        cursor
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

/// Read-only target chip shown when the dialog was opened from a
/// repo/workspace context menu — the target is implied by where the user
/// invoked the dialog, so the picker is replaced by a name label. The
/// label still occupies the leading slot in the form so users have a
/// confirmation of which container they're spawning into before they
/// commit.
function ImpliedTargetLabel({
  target,
  repos,
  workspaces,
}: {
  target: TargetSelection;
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
}) {
  const label =
    target.kind === "repo"
      ? (repos.find((r) => r.id === target.id)?.name ?? target.id)
      : (workspaces.find((w) => w.id === target.id)?.name ?? target.id);
  const kindTag = target.kind === "repo" ? "[REPO]" : "[WS]";
  return (
    <div className="field" data-testid="spawn-target-implied">
      <span>Target</span>
      <div className="spawn-target-implied-value">
        <span className="muted small">{kindTag}</span>
        <strong>{label}</strong>
      </div>
    </div>
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
    <div className="field">
      <span>Open in</span>
      <div
        className="segmented spawn-placement-segmented"
        role="radiogroup"
        aria-label="Open in"
      >
        <button
          type="button"
          role="radio"
          aria-checked={value.kind === "current_tab"}
          className={value.kind === "current_tab" ? "active" : ""}
          disabled={!canUseCurrentTab}
          onClick={() => onChange({ kind: "current_tab" })}
          title={
            canUseCurrentTab
              ? undefined
              : "The current tab cannot host terminal panes"
          }
          data-testid="spawn-placement-current-tab"
        >
          Current tab
          {currentTabName && (
            <span className="segmented-hint">{currentTabName}</span>
          )}
        </button>
        <button
          type="button"
          role="radio"
          aria-checked={value.kind === "new_tab"}
          className={value.kind === "new_tab" ? "active" : ""}
          onClick={() => onChange({ kind: "new_tab" })}
          data-testid="spawn-placement-new-tab"
        >
          New tab
        </button>
        {otherTabs.map((t) => (
          <button
            key={`placement:${t.id}`}
            type="button"
            role="radio"
            aria-checked={value.kind === "tab" && value.tabId === t.id}
            className={
              value.kind === "tab" && value.tabId === t.id ? "active" : ""
            }
            onClick={() => onChange({ kind: "tab", tabId: t.id })}
            data-testid={`spawn-placement-tab-${t.id}`}
          >
            {t.name}
          </button>
        ))}
      </div>
    </div>
  );
}

/// Branch field shared by both forms. Behavior:
///   * When editing a saved spawn config, seed with its saved branch name.
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

/// Given the repo's local default branch and the remote-tracking refs the
/// daemon reported, pick the ref a new worktree should be based on.
///
/// Prefers `origin/<default>` when it exists. In a repo driven through
/// worktrees the local default never advances — every session forks off it,
/// works, and pushes — so seeding the field with the local name silently
/// bases new work on a months-old commit. Seeding with the remote ref makes
/// the choice visible and editable rather than hidden.
function preferRemoteBase(
  localDefault: string,
  remoteBranches: string[],
): string {
  const candidates = remoteBranches.filter(
    (r) => r.slice(r.indexOf("/") + 1) === localDefault,
  );
  // Prefer `origin` when several remotes carry the branch, matching the
  // daemon's own resolution order — otherwise the field could seed with a
  // different remote than the one the spawn would actually use.
  return (
    candidates.find((r) => r.startsWith("origin/")) ??
    candidates[0] ??
    localDefault
  );
}

/// Staleness callout for a base branch that trails its remote counterpart.
/// Renders nothing when the base is level, unknown, or already a remote ref.
function BaseStalenessNotice({
  preview,
  testId,
}: {
  preview: MemberSpawnPreview | null;
  testId: string;
}) {
  const behind = preview?.base_behind_remote ?? 0;
  if (!preview || behind <= 0 || !preview.base_remote_ref) return null;
  return (
    <div className="inline-warning" data-testid={testId}>
      <strong>{preview.effective_base}</strong> is {behind} commit
      {behind === 1 ? "" : "s"} behind{" "}
      <strong>{preview.base_remote_ref}</strong>. Branching from it forks from
      that older point.
    </div>
  );
}

/// Collision callout plus the reuse/recreate choice, shown when a worktree
/// already sits at the path this spawn would use — or when only the branch
/// survives (the leftover a discarded session leaves behind), which a fresh
/// worktree add would silently attach at its old tip.
///
/// Reuse is the default and keeps the historical behavior; the important part
/// is that it stops being silent, since a reused worktree or branch ignores
/// the base branch entirely and keeps whatever fork point it was created with.
function WorktreeCollisionNotice({
  preview,
  policy,
  onPolicyChange,
  idPrefix,
}: {
  preview: MemberSpawnPreview | null;
  policy: WorktreeReuseChoice;
  onPolicyChange: (next: WorktreeReuseChoice) => void;
  idPrefix: string;
}) {
  if (!preview) return null;
  const branchOnly = !preview.worktree_exists && preview.branch_exists;
  if (!preview.worktree_exists && !branchOnly) return null;
  const behind = branchOnly
    ? (preview.existing_branch_behind_base ?? 0)
    : (preview.existing_worktree_behind_base ?? 0);
  const head = branchOnly
    ? preview.existing_branch_head
    : preview.existing_worktree_head;
  return (
    <div className="inline-warning" data-testid={`${idPrefix}-collision`}>
      <div>
        {branchOnly
          ? "A branch with this name already exists (its worktree was deleted)"
          : "A worktree already exists at this path"}
        {head ? (
          <>
            {" "}
            at <code>{head}</code>
          </>
        ) : null}
        {behind > 0 ? (
          <>
            , {behind} commit{behind === 1 ? "" : "s"} behind{" "}
            <strong>{preview.resolved_base_ref ?? "the base branch"}</strong>
          </>
        ) : null}
        .
      </div>
      <label className="radio-row">
        <input
          type="radio"
          name={`${idPrefix}-reuse`}
          checked={policy === "reuse"}
          onChange={() => onPolicyChange("reuse")}
          data-testid={`${idPrefix}-collision-reuse`}
        />
        <span>
          {branchOnly ? "Attach it as-is" : "Reuse it as-is"}
          <span className="muted small inline-note">
            (keeps its existing fork point; the base branch is not applied)
          </span>
        </span>
      </label>
      <label className="radio-row">
        <input
          type="radio"
          name={`${idPrefix}-reuse`}
          checked={policy === "recreate_from_base"}
          onChange={() => onPolicyChange("recreate_from_base")}
          data-testid={`${idPrefix}-collision-recreate`}
        />
        <span>
          Recreate from the base branch
          {!branchOnly && preview.existing_worktree_dirty ? (
            <span className="danger small inline-note">
              (discards uncommitted changes in that worktree)
            </span>
          ) : (
            <span className="muted small inline-note">
              {branchOnly
                ? "(deletes the old branch and forks fresh)"
                : "(deletes and re-adds the worktree)"}
            </span>
          )}
        </span>
      </label>
    </div>
  );
}

function useBranchField(
  defaultBranch: string,
  useWorktree: boolean,
  targetKey: string,
  prefillValue: string | null,
) {
  const initial = prefillValue ?? (useWorktree
    ? (branchSuggestionCache.get(targetKey) ?? randomWorktreeBranchName())
    : defaultBranch);
  if (
    prefillValue === null &&
    useWorktree &&
    !branchSuggestionCache.has(targetKey)
  ) {
    branchSuggestionCache.set(targetKey, initial);
  }
  const [value, setValue] = useState<string>(initial);
  // Track whether the random rule should still fire. Reset to true whenever
  // the entity (and therefore `defaultBranch`) changes; flipped to false the
  // moment the user edits the field by hand.
  const allowAutoRef = useRef(prefillValue === null);

  useEffect(() => {
    allowAutoRef.current = prefillValue === null;
    if (prefillValue !== null) {
      setValue(prefillValue);
      return;
    }
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
  }, [defaultBranch, targetKey, prefillValue]);

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
  spawnPrefill,
  spawnPin,
  spawnPlacement,
  onClose,
  onSpawned,
  onSpawnRequest,
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
  spawnPrefill: SpawnConfig | undefined;
  spawnPin: WorktreeLaunchTarget | undefined;
  spawnPlacement: SpawnPlacement;
  onClose: () => void;
  onSpawned: (placement: SpawnPlacement) => void;
  onSpawnRequest: (
    req: Extract<ClientMessage, { type: "spawn_session" }>,
  ) => void;
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
  const prefillTarget =
    spawnPrefill?.target.kind === "single" &&
    spawnPrefill.target.repo_id === repoId
      ? spawnPrefill.target
      : null;

  const [knownBranches, setKnownBranches] = useState<string[]>([]);
  const [remoteBranches, setRemoteBranches] = useState<string[]>([]);
  const [currentBranch, setCurrentBranch] = useState<string | null>(null);
  useEffect(() => {
    if (!repoId) return;
    setKnownBranches([]);
    setRemoteBranches([]);
    setCurrentBranch(null);
    client.send({ type: "list_branches", repo_id: repoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "branches" || detail.repo_id !== repoId) return;
      setKnownBranches(detail.branches);
      setRemoteBranches(detail.remote_branches);
      setCurrentBranch(detail.current);
    };
    window.addEventListener("rt:branches", handler);
    return () => window.removeEventListener("rt:branches", handler);
  }, [repoId, client]);

  // Refresh remote-tracking refs in the background when the dialog opens.
  // Deliberately off the spawn path: a slow or offline remote must never
  // delay a launch, so the fetch just re-lists branches when it lands and
  // the preview recomputes against the newer refs.
  const [fetchState, setFetchState] = useState<"idle" | "running" | "failed">(
    "idle",
  );
  useEffect(() => {
    if (!repoId) return;
    setFetchState("running");
    client.send({ type: "fetch_repo", repo_id: repoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "repo_fetched" || detail.repo_id !== repoId) return;
      setFetchState(detail.error ? "failed" : "idle");
      if (!detail.error) {
        client.send({ type: "list_branches", repo_id: repoId });
      }
    };
    window.addEventListener("rt:repo_fetched", handler);
    return () => window.removeEventListener("rt:repo_fetched", handler);
  }, [repoId, client]);

  // Fallback chain for the in-place / base-branch field: repo's persisted
  // default → freshly-reported current branch → first known branch → "main"
  // literal. The literal is only reached when both the daemon and `git`
  // couldn't tell us anything; historically we landed here for repos whose
  // initial default-detection failed and stuck with `default_branch=null`,
  // which then crashed `git checkout -b main main` because the base ref
  // didn't exist.
  const localDefaultBranch =
    repo?.default_branch ?? currentBranch ?? knownBranches[0] ?? "main";
  // Worktree base defaults to the *remote* counterpart when there is one —
  // see preferRemoteBase. Shown in the field rather than applied invisibly,
  // so a user who genuinely wants the local ref can just edit it back.
  const defaultBranch = useMemo(
    () => preferRemoteBase(localDefaultBranch, remoteBranches),
    [localDefaultBranch, remoteBranches],
  );
  // The in-place "Branch" field defaults to the branch you're already on (so
  // spawning in place is a no-op checkout). It stays a local name — you can't
  // check out a remote-tracking ref in place without detaching HEAD.
  const inPlaceDefault =
    currentBranch ?? repo?.default_branch ?? knownBranches[0] ?? "main";
  /// Worktree this dialog was opened pinned to ("Launch session here" in the
  /// worktrees manager). Non-null means the mode toggle and picker start on
  /// that worktree instead of on a fresh one.
  const pinned =
    spawnPin?.kind === "single" && spawnPin.repo_id === repoId
      ? spawnPin
      : null;

  const [useWorktree, setUseWorktree] = useState<boolean>(
    () => pinned !== null || (prefillTarget?.use_worktree ?? true),
  );
  /// "new" creates a fresh worktree (auto-named or user-typed branch).
  /// "existing" picks an already-checked-out worktree from this repo —
  /// useful for returning to a parked session's worktree, or one left behind
  /// by a session that has since been discarded.
  const [worktreeMode, setWorktreeMode] = useState<"new" | "existing">(
    pinned !== null ? "existing" : "new",
  );
  const [worktreeList, setWorktreeList] = useState<WorktreeInfo[]>([]);
  /// False until the daemon's reply lands, so an empty list can say "none"
  /// rather than claiming to still be loading forever.
  const [worktreesLoaded, setWorktreesLoaded] = useState(false);
  /// Absolute path of the picked worktree. The spawn is addressed by path
  /// rather than by branch name: deriving the directory from the branch
  /// silently creates a *second* worktree whenever the picked one has since
  /// been switched to another branch or left on a detached HEAD.
  const [selectedWorktree, setSelectedWorktree] = useState<string | null>(
    pinned?.worktree_path ?? null,
  );

  // Reset the persistent-preference seed when the chosen repo changes.
  useEffect(() => {
    setUseWorktree(
      pinned !== null ||
        (prefillTarget?.use_worktree ?? repo?.default_use_worktree ?? true),
    );
    // Flip back to "new" on repo change — the existing worktree list
    // belongs to the previous repo and would be misleading.
    setWorktreeMode(pinned !== null ? "existing" : "new");
    setSelectedWorktree(pinned?.worktree_path ?? null);
    setWorktreeList([]);
    setWorktreesLoaded(false);
  }, [repo, prefillTarget?.use_worktree, pinned]);

  // Fetch the worktree list when entering "existing" mode for the current
  // repo. Mirrors the list_branches pattern above.
  useEffect(() => {
    if (!repoId || !useWorktree || worktreeMode !== "existing") return;
    setWorktreeList([]);
    setWorktreesLoaded(false);
    client.send({ type: "list_worktrees", repo_id: repoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "worktrees" || detail.repo_id !== repoId) return;
      setWorktreeList(detail.worktrees);
      setWorktreesLoaded(true);
    };
    window.addEventListener("rt:worktrees", handler);
    return () => window.removeEventListener("rt:worktrees", handler);
  }, [repoId, useWorktree, worktreeMode, client]);

  // Adopt the list's spelling of the selected path once it arrives. A pin from
  // the worktrees manager carries the scan's backslash form while the picker's
  // options carry git's forward-slash form; without this the <select> can't
  // match its own value and falls back to the placeholder.
  useEffect(() => {
    if (selectedWorktree === null) return;
    const match = worktreeList.find((w) => samePath(w.path, selectedWorktree));
    if (match && match.path !== selectedWorktree) setSelectedWorktree(match.path);
  }, [worktreeList, selectedWorktree]);

  const branch = useBranchField(
    inPlaceDefault,
    useWorktree,
    `repo:${repoId}`,
    prefillTarget?.branch_name ?? null,
  );

  const [baseBranch, setBaseBranchValue] = useState<string>(
    () => prefillTarget?.base_branch ?? "",
  );
  // Auto-seeding stops the moment the user edits the field, and resumes on a
  // repo switch. `defaultBranch` settles asynchronously — the branch list
  // arrives over the WS, and the background fetch can revise it a second time
  // seconds later — so without this the seed effect overwrites a base branch
  // the user has already typed.
  const baseBranchTouchedFor = useRef<string | null>(null);
  const setBaseBranch = (value: string) => {
    baseBranchTouchedFor.current = repoId;
    setBaseBranchValue(value);
  };
  useEffect(() => {
    if (baseBranchTouchedFor.current === repoId) return;
    setBaseBranchValue(prefillTarget?.base_branch ?? defaultBranch);
  }, [repoId, defaultBranch, prefillTarget?.base_branch]);

  // Where this spawn would actually fork from. Requested whenever the inputs
  // that determine it change, so the user sees the fork point before
  // committing rather than discovering it days later.
  const [preview, setPreview] = useState<MemberSpawnPreview | null>(null);
  const [reusePolicy, setReusePolicy] = useState<WorktreeReuseChoice>("reuse");
  const trimmedBranch = branch.value.trim();
  useEffect(() => {
    setPreview(null);
    setReusePolicy("reuse");
    if (!repoId || !trimmedBranch || worktreeMode === "existing") return;
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "spawn_preview" ||
        detail.repo_id !== repoId ||
        detail.branch_name !== trimmedBranch
      ) {
        return;
      }
      setPreview(detail.preview);
    };
    window.addEventListener("rt:spawn_preview", handler);
    // Debounced so typing a branch name doesn't fire a git-backed preview per
    // keystroke.
    const timer = window.setTimeout(() => {
      client.send({
        type: "preview_spawn",
        repo_id: repoId,
        branch_name: trimmedBranch,
        base_branch: baseBranch.trim() || null,
        use_worktree: useWorktree,
      });
    }, 250);
    return () => {
      window.clearTimeout(timer);
      window.removeEventListener("rt:spawn_preview", handler);
    };
  }, [
    repoId,
    trimmedBranch,
    baseBranch,
    useWorktree,
    worktreeMode,
    fetchState,
    client,
  ]);

  const selectedInfo = useMemo(
    () => worktreeList.find((w) => samePath(w.path, selectedWorktree)) ?? null,
    [worktreeList, selectedWorktree],
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
  const [shareConfirmOpen, setShareConfirmOpen] = useState(false);

  const pinningExisting = useWorktree && worktreeMode === "existing";
  /// In "existing" mode something must actually be picked. The daemon
  /// validates the directory itself, so list membership isn't required —
  /// a pin can name a worktree git has since pruned from its own registry.
  const existingWorktreeOk = !pinningExisting || selectedWorktree !== null;

  const canSubmit =
    !!repoId &&
    (pinningExisting || branch.value.trim().length > 0) &&
    existingWorktreeOk &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true) &&
    envRowsAreValid(advanced.envRows);
  const effectiveBaseBranch =
    useWorktree && worktreeMode === "new"
      ? baseBranch.trim() || null
      : null;

  /// Label the pinned session with the branch its worktree is really on. The
  /// daemon re-reads HEAD and wins over this, but it needs a non-empty
  /// fallback for a detached worktree.
  const pinnedBranchName = () =>
    pinned?.branch ??
    selectedInfo?.branch ??
    branchFromWorktreePath(selectedInfo?.group_path ?? selectedWorktree);

  const submit = () => {
    if (!canSubmit || submittedRef.current) return;
    // Two agents in one working tree is supported but rarely intended.
    if (pinningExisting && selectedInfo?.status.kind === "active") {
      setShareConfirmOpen(true);
      return;
    }
    sendSpawn();
  };

  const sendSpawn = () => {
    if (submittedRef.current) return;
    submittedRef.current = true;
    // Successful submit consumes the cached branch suggestion so the
    // next dialog open generates a fresh one rather than re-pinning the
    // name the user just spawned with.
    branchSuggestionCache.delete(`repo:${repoId}`);
    // Persist the worktree-toggle choice as the new default for this repo
    // only on successful submit, so cancellation leaves the setting intact.
    if (repo && useWorktree !== (repo.default_use_worktree ?? true)) {
      client.send({
        type: "set_repo_worktree_default",
        repo_id: repo.id,
        value: useWorktree,
      });
    }
    const spawnMsg: Extract<ClientMessage, { type: "spawn_session" }> = {
      type: "spawn_session",
      label: null,
      target: {
        kind: "single",
        repo_id: repoId,
        branch_name: pinningExisting
          ? pinnedBranchName()
          : branch.value.trim(),
        base_branch: effectiveBaseBranch,
        use_worktree: useWorktree,
        // First attempt carries no strategy: a dirty in-place switch makes the
        // daemon ask via `checkout_confirm_required`, and the app resends this
        // request with the chosen strategy.
        checkout_strategy: null,
        worktree_reuse: reusePolicy,
        existing_worktree: pinningExisting ? selectedWorktree : null,
      },
      mode: runMode,
      initial_prompt: runMode === "headless" ? headlessPrompt.trim() : null,
      dangerously_skip_permissions:
        runMode === "plain_shell" ? false : skipPerms,
      agent_options: buildAgentOptions(
        agent,
        advanced,
        skipPerms,
        runMode === "plain_shell",
      ),
      ...advancedToWire(advanced, runMode),
    };
    client.send(spawnMsg);
    onSpawnRequest(spawnMsg);
    onSpawned(spawnPlacement);
    onClose();
  };

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
        <>
          <label className="field">
            <span>Existing worktree</span>
            <select
              value={selectedWorktree ?? ""}
              onChange={(e) => setSelectedWorktree(e.target.value || null)}
              data-testid="spawn-single-worktree-picker"
            >
              <option value="" disabled>
                {worktreeList.length > 0
                  ? "Choose a worktree…"
                  : worktreesLoaded
                    ? "This repo has no worktrees"
                    : "Loading…"}
              </option>
              {/* A pin can name a worktree git has pruned from its registry,
                  so it isn't necessarily in the list — keep it selectable. */}
              {pinned !== null &&
                !worktreeList.some((w) =>
                  samePath(w.path, pinned.worktree_path),
                ) && (
                  <option value={pinned.worktree_path}>
                    {pinned.branch ?? branchFromWorktreePath(pinned.worktree_path)}
                  </option>
                )}
              {worktreeList.map((w) => (
                <option key={w.path} value={w.path} title={w.path}>
                  {worktreeOptionLabel(w)}
                </option>
              ))}
            </select>
          </label>
          {selectedWorktree !== null && (
            <div className="muted small" data-testid="spawn-single-worktree-path">
              Runs in <code>{selectedWorktree}</code>
            </div>
          )}
          {selectedInfo?.status.kind === "active" && (
            <div
              className="warning-text small"
              data-testid="spawn-single-worktree-active-warning"
            >
              A session is already running in this worktree. Launching puts a
              second agent in the same working tree.
            </div>
          )}
        </>
      ) : (
        <>
          <label className="field">
            <span>{useWorktree ? "New worktree branch" : "Branch"}</span>
            <div className="branch-with-dice">
              <BranchCombobox
                value={branch.value}
                onChange={branch.setValue}
                branches={knownBranches}
                currentBranch={currentBranch}
                placeholder={useWorktree ? localDefaultBranch : inPlaceDefault}
                inputRef={branchInputRef}
                testId="spawn-single-branch"
              />
              {useWorktree && (
                <button
                  type="button"
                  className="branch-dice"
                  onClick={() => {
                    branch.setValue(randomWorktreeBranchName());
                    branchInputRef.current?.focus();
                  }}
                  title="Generate a random worktree branch name"
                  aria-label="Generate a random worktree branch name"
                  data-testid="spawn-single-branch-random"
                >
                  Random
                </button>
              )}
            </div>
          </label>

          {useWorktree && worktreeMode === "new" && (
            <>
              <label className="field">
                <span>Base branch (optional)</span>
                <input
                  type="text"
                  value={baseBranch}
                  onChange={(e) => setBaseBranch(e.target.value)}
                  placeholder={defaultBranch}
                  list={`spawn-single-base-options-${repoId}`}
                  data-testid="spawn-single-base-branch"
                />
                <datalist id={`spawn-single-base-options-${repoId}`}>
                  {remoteBranches.map((b) => (
                    <option key={`remote-${b}`} value={b} />
                  ))}
                  {knownBranches.map((b) => (
                    <option key={`local-${b}`} value={b} />
                  ))}
                </datalist>
              </label>
              {fetchState === "failed" && (
                <div className="muted small" data-testid="spawn-single-fetch-failed">
                  Couldn't reach the remote — comparing against the last
                  fetched refs.
                </div>
              )}
              <BaseStalenessNotice
                preview={preview}
                testId="spawn-single-base-stale"
              />
              <WorktreeCollisionNotice
                preview={preview}
                policy={reusePolicy}
                onPolicyChange={setReusePolicy}
                idPrefix="spawn-single"
              />
            </>
          )}
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
      {shareConfirmOpen && (
        <ShareWorktreeConfirm
          onCancel={() => setShareConfirmOpen(false)}
          onConfirm={() => {
            setShareConfirmOpen(false);
            sendSpawn();
          }}
        />
      )}
    </>
  );
}

/// Second click required before two agents end up in one working tree. Shown
/// by both spawn forms when the picked worktree already has a live session.
function ShareWorktreeConfirm({
  onCancel,
  onConfirm,
}: {
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="modal-backdrop" data-testid="spawn-share-worktree-confirm">
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <header className="modal-header">
          <h2>Share this worktree?</h2>
        </header>
        <div className="modal-body">
          <p>
            A session is already running in the worktree you picked. Both agents
            will see each other's uncommitted edits, and concurrent writes to
            the same file will overwrite one another.
          </p>
        </div>
        <footer className="modal-footer">
          <button
            type="button"
            onClick={onCancel}
            data-testid="spawn-share-worktree-cancel"
          >
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            onClick={onConfirm}
            data-testid="spawn-share-worktree-ok"
          >
            Launch anyway
          </button>
        </footer>
      </div>
    </div>
  );
}

/// Compare two worktree paths for identity. `git worktree list` prints forward
/// slashes even on Windows, while the worktrees-root scan a pin comes from
/// yields backslashes — so the same directory arrives spelled two ways and a
/// raw string compare would show it twice in the picker.
function samePath(a: string | null, b: string | null): boolean {
  if (a === null || b === null) return false;
  const norm = (p: string) => {
    const trimmed = p.replace(/[/\\]+$/, "");
    return isWindowsPath(trimmed)
      ? trimmed.toLowerCase().replace(/\\/g, "/")
      : trimmed;
  };
  return norm(a) === norm(b);
}

/// A drive-letter or UNC prefix means Windows semantics — case-insensitive and
/// separator-agnostic. Elsewhere a backslash is a legal filename character, so
/// collapsing it would conflate genuinely different paths.
function isWindowsPath(p: string): boolean {
  return /^[a-zA-Z]:[/\\]/.test(p) || p.startsWith("\\\\");
}

/// Best-effort branch name for a worktree the daemon didn't report one for:
/// strip the `wt.` prefix off a group directory, else use the path leaf. Only
/// ever a label fallback — the daemon reads the real HEAD at spawn time.
function branchFromWorktreePath(path: string | null): string {
  const leaf = (path ?? "").split(/[\\/]/).filter(Boolean).pop() ?? "";
  if (leaf.startsWith("wt.")) return leaf.slice(3);
  return leaf || "worktree";
}

/// One picker row: branch (or a detached-HEAD marker), plus staleness, size,
/// and age for worktrees RT manages. Non-RT worktrees show branch only.
function worktreeOptionLabel(w: WorktreeInfo): string {
  const name = w.branch || "(detached HEAD)";
  const meta: string[] = [];
  switch (w.status.kind) {
    case "active":
      meta.push("in use");
      break;
    case "detached":
      meta.push("stopped session");
      break;
    case "stale":
      // Only meaningful for a worktree RT manages; a hand-made one has no
      // group to be stale relative to.
      if (w.group_path) meta.push("stale");
      break;
    case "unknown":
      break;
  }
  if (w.size_bytes != null) meta.push(humanSize(w.size_bytes));
  if (w.last_modified_unix != null) {
    meta.push(humanRelativeTime(w.last_modified_unix));
  }
  return meta.length > 0 ? `${name} — ${meta.join(", ")}` : name;
}

function humanSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(1)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  return `${(mb / 1024).toFixed(2)} GB`;
}

function humanRelativeTime(unixSeconds: number): string {
  const delta = Math.floor(Date.now() / 1000) - unixSeconds;
  if (delta < 60) return "just now";
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
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
  spawnPrefill,
  spawnPin,
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
  spawnPrefill: SpawnConfig | undefined;
  spawnPin: WorktreeLaunchTarget | undefined;
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

  const prefillTarget =
    spawnPrefill?.target.kind === "workspace" &&
    spawnPrefill.target.workspace_id === workspaceId
      ? spawnPrefill.target
      : null;

  const pinned =
    spawnPin?.kind === "workspace" && spawnPin.workspace_id === workspaceId
      ? spawnPin
      : null;

  /// "existing" binds every member to a worktree group that already exists on
  /// disk. Members the group has no worktree for still get one created, at the
  /// derived path — which lands inside the same group.
  const [worktreeMode, setWorktreeMode] = useState<"new" | "existing">(
    pinned !== null ? "existing" : "new",
  );
  const [groupList, setGroupList] = useState<WorkspaceGroupOption[]>([]);
  const [selectedGroupKey, setSelectedGroupKey] = useState<string | null>(
    pinned !== null ? groupKey(pinned.members) : null,
  );

  // The worktrees-root snapshot is the only source that knows which groups map
  // to this workspace — `list_worktrees` is per-repo and can't tell a
  // workspace group from a single-repo one.
  useEffect(() => {
    if (worktreeMode !== "existing") return;
    client.send({ type: "inspect_worktrees_root" });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "worktrees_root_snapshot") return;
      setGroupList(workspaceGroupOptions(detail.entries, workspaceId));
    };
    window.addEventListener("rt:worktrees_root_snapshot", handler);
    return () =>
      window.removeEventListener("rt:worktrees_root_snapshot", handler);
  }, [worktreeMode, workspaceId, client]);

  const selectedGroup = useMemo(() => {
    const found = groupList.find((g) => g.key === selectedGroupKey);
    if (found) return found;
    // A pin stays selectable before (or without) a snapshot arriving.
    if (pinned && selectedGroupKey === groupKey(pinned.members)) {
      return {
        key: groupKey(pinned.members),
        label: pinned.branch ?? branchFromWorktreePath(pinned.members[0]?.path ?? null),
        branch: pinned.branch,
        members: pinned.members,
        status: { kind: "stale" } as RootWorktreeStatus,
      } satisfies WorkspaceGroupOption;
    }
    return null;
  }, [groupList, selectedGroupKey, pinned]);

  // Background-fetch every member so the preview's staleness figures reflect
  // current remotes. Same non-blocking contract as SingleForm: a failure just
  // means the comparison uses cached refs.
  const memberIds = workspace?.member_repo_ids;
  useEffect(() => {
    if (!memberIds) return;
    for (const id of memberIds) {
      client.send({ type: "fetch_repo", repo_id: id });
    }
  }, [memberIds, client]);

  // Remote-tracking refs for the first member drive the base-branch default.
  // Members are expected to share a default branch name; the daemon resolves
  // each member's base independently at spawn time regardless.
  const [remoteBranches, setRemoteBranches] = useState<string[]>([]);
  useEffect(() => {
    const id = firstMember?.id;
    if (!id) return;
    setRemoteBranches([]);
    client.send({ type: "list_branches", repo_id: id });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "branches" || detail.repo_id !== id) return;
      setRemoteBranches(detail.remote_branches);
    };
    window.addEventListener("rt:branches", handler);
    return () => window.removeEventListener("rt:branches", handler);
  }, [firstMember?.id, client]);

  const localDefaultBranch = firstMember?.default_branch ?? "main";
  const defaultBranch = useMemo(
    () => preferRemoteBase(localDefaultBranch, remoteBranches),
    [localDefaultBranch, remoteBranches],
  );
  const [useWorktree, setUseWorktree] = useState<boolean>(
    () => prefillTarget?.use_worktree ?? true,
  );

  useEffect(() => {
    setUseWorktree(
      prefillTarget?.use_worktree ?? workspace?.default_use_worktree ?? true,
    );
  }, [workspace, prefillTarget?.use_worktree]);

  // The branch NAME field must seed with the local default: `defaultBranch`
  // prefers the remote-tracking ref (`origin/main`), which is only ever a
  // valid *base*. Naming a branch after it creates a local
  // `refs/heads/origin/main` and makes the ref ambiguous repo-wide.
  const branch = useBranchField(
    localDefaultBranch,
    useWorktree,
    `workspace:${workspaceId}`,
    prefillTarget?.branch_name ?? null,
  );

  const [baseBranch, setBaseBranchValue] = useState<string>(
    () => prefillTarget?.base_branch ?? "",
  );
  // See SingleForm: the background fetch revises `defaultBranch` after the
  // dialog is already open, so seeding has to stop once the user has typed.
  const baseBranchTouchedFor = useRef<string | null>(null);
  const setBaseBranch = (value: string) => {
    baseBranchTouchedFor.current = workspaceId;
    setBaseBranchValue(value);
  };
  useEffect(() => {
    if (baseBranchTouchedFor.current === workspaceId) return;
    setBaseBranchValue(prefillTarget?.base_branch ?? defaultBranch);
  }, [workspaceId, defaultBranch, prefillTarget?.base_branch]);

  const [preview, setPreview] = useState<MemberSpawnPreview[] | null>(null);
  const [reusePolicy, setReusePolicy] = useState<WorktreeReuseChoice>("reuse");
  useEffect(() => {
    // Clear the preview whenever any input that changes the worktree-add
    // plan changes — that includes `useWorktree`, which alters paths. Audit
    // finding: previously the preview went stale when the user toggled
    // worktree mode after a Preview, leaving the table showing paths that
    // no longer matched the active mode.
    setPreview(null);
    // A recreate decision belongs to the collision the user was shown; a
    // changed plan means a different collision (or none).
    setReusePolicy("reuse");
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
    if (!workspaceId || !branch.value || !useWorktree) return;
    client.send({
      type: "preview_workspace_spawn",
      workspace_id: workspaceId,
      branch_name: branch.value,
      base_branch: baseBranch.trim() || null,
    });
  };

  // Mirror SingleForm's double-click guard.
  const submittedRef = useRef(false);
  const [shareConfirmOpen, setShareConfirmOpen] = useState(false);

  const pinningExisting = useWorktree && worktreeMode === "existing";
  const canSpawn =
    !!workspaceId &&
    (pinningExisting
      ? selectedGroup !== null
      : branch.value.trim().length > 0) &&
    (runMode === "headless" ? headlessPrompt.trim().length > 0 : true) &&
    envRowsAreValid(advanced.envRows);
  const effectiveBaseBranch =
    useWorktree && !pinningExisting ? baseBranch.trim() || null : null;

  const submit = () => {
    if (!canSpawn || submittedRef.current) return;
    if (pinningExisting && selectedGroup?.status.kind === "active") {
      setShareConfirmOpen(true);
      return;
    }
    sendSpawn();
  };

  const sendSpawn = () => {
    if (submittedRef.current) return;
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
        // A pinned launch labels itself with the group's branch; members the
        // group lacks derive their new worktree from the same name, which
        // keeps them inside the same wt.<slug>/ directory.
        branch_name: pinningExisting
          ? (selectedGroup?.branch ?? selectedGroup?.label ?? "")
          : branch.value.trim(),
        base_branch: effectiveBaseBranch,
        use_worktree: useWorktree,
        worktree_reuse: reusePolicy,
        existing_worktrees: pinningExisting
          ? (selectedGroup?.members ?? [])
          : [],
      },
      mode: runMode,
      initial_prompt: runMode === "headless" ? headlessPrompt.trim() : null,
      dangerously_skip_permissions:
        runMode === "plain_shell" ? false : skipPerms,
      agent_options: buildAgentOptions(
        agent,
        advanced,
        skipPerms,
        runMode === "plain_shell",
      ),
      ...advancedToWire(advanced, runMode),
    });
    onSpawned(spawnPlacement);
    onClose();
  };

  return (
    <>
      {!pinningExisting && (
        <label className="field">
          <span>
            {useWorktree
              ? "New worktree branch (same across all members)"
              : "Branch (same across all members)"}
          </span>
          <div className="branch-with-dice">
            <input
              ref={branchInputRef}
              type="text"
              value={branch.value}
              onChange={(e) => branch.setValue(e.target.value)}
              placeholder={localDefaultBranch}
              data-testid="spawn-workspace-branch"
            />
            {useWorktree && (
              <button
                type="button"
                className="branch-dice"
                onClick={() => {
                  branch.setValue(randomWorktreeBranchName());
                  branchInputRef.current?.focus();
                }}
                title="Generate a random worktree branch name"
                aria-label="Generate a random worktree branch name"
                data-testid="spawn-workspace-branch-random"
              >
                Random
              </button>
            )}
          </div>
        </label>
      )}

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

      {useWorktree && (
        <div className="field" role="radiogroup" aria-label="Worktree mode">
          <span>Worktrees</span>
          <div className="segmented">
            <button
              type="button"
              className={worktreeMode === "new" ? "active" : ""}
              onClick={() => setWorktreeMode("new")}
              data-testid="spawn-workspace-worktree-mode-new"
            >
              New worktrees
            </button>
            <button
              type="button"
              className={worktreeMode === "existing" ? "active" : ""}
              onClick={() => setWorktreeMode("existing")}
              data-testid="spawn-workspace-worktree-mode-existing"
            >
              Use existing
            </button>
          </div>
        </div>
      )}

      {pinningExisting && (
        <>
          <label className="field">
            <span>Existing worktree group</span>
            <select
              value={selectedGroupKey ?? ""}
              onChange={(e) => setSelectedGroupKey(e.target.value || null)}
              data-testid="spawn-workspace-worktree-picker"
            >
              <option value="" disabled>
                {groupList.length === 0
                  ? "No worktree groups for this workspace"
                  : "Choose a worktree group…"}
              </option>
              {selectedGroup !== null &&
                !groupList.some((g) => g.key === selectedGroup.key) && (
                  <option value={selectedGroup.key}>
                    {selectedGroup.label}
                  </option>
                )}
              {groupList.map((g) => (
                <option key={g.key} value={g.key}>
                  {g.label}
                </option>
              ))}
            </select>
          </label>
          {selectedGroup !== null && (
            <div className="muted small" data-testid="spawn-workspace-worktree-members">
              {selectedGroup.members.length} member
              {selectedGroup.members.length === 1 ? "" : "s"} bound;
              {" "}
              {Math.max(
                (workspace?.member_repo_ids.length ?? 0) -
                  selectedGroup.members.length,
                0,
              )}{" "}
              to be created
            </div>
          )}
          {selectedGroup?.status.kind === "active" && (
            <div
              className="warning-text small"
              data-testid="spawn-workspace-worktree-active-warning"
            >
              A session is already running in this group. Launching puts a
              second agent in the same working trees.
            </div>
          )}
        </>
      )}

      {useWorktree && !pinningExisting && (
        <>
          <label className="field">
            <span>Base branch (optional)</span>
            <input
              type="text"
              value={baseBranch}
              onChange={(e) => setBaseBranch(e.target.value)}
              placeholder={defaultBranch}
              list={`spawn-workspace-base-options-${workspaceId}`}
              data-testid="spawn-workspace-base-branch"
            />
            <datalist id={`spawn-workspace-base-options-${workspaceId}`}>
              {remoteBranches.map((b) => (
                <option key={`remote-${b}`} value={b} />
              ))}
            </datalist>
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
        </>
      )}

      {useWorktree && !pinningExisting && preview && (
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
                    {m.worktree_exists && (
                      <span className="badge badge-warn">
                        worktree exists
                        {m.existing_worktree_behind_base
                          ? ` (${m.existing_worktree_behind_base} behind)`
                          : ""}
                      </span>
                    )}
                    {(m.base_behind_remote ?? 0) > 0 && (
                      <span className="badge badge-warn">
                        base {m.base_behind_remote} behind {m.base_remote_ref}
                      </span>
                    )}
                  </td>
                  <td className="muted small">{m.worktree_path}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <WorktreeCollisionNotice
            preview={
              preview.find((m) => m.worktree_exists || m.branch_exists) ?? null
            }
            policy={reusePolicy}
            onPolicyChange={setReusePolicy}
            idPrefix="spawn-workspace"
          />
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
      {shareConfirmOpen && (
        <ShareWorktreeConfirm
          onCancel={() => setShareConfirmOpen(false)}
          onConfirm={() => {
            setShareConfirmOpen(false);
            sendSpawn();
          }}
        />
      )}
    </>
  );
}

/// One row of the workspace form's existing-group picker.
interface WorkspaceGroupOption {
  /// Stable identity: the member paths, which is what actually gets pinned.
  key: string;
  label: string;
  branch: string | null;
  members: PinnedMemberWorktree[];
  status: RootWorktreeStatus;
}

function groupKey(members: PinnedMemberWorktree[]): string {
  return members.map((m) => m.path).join("|");
}

/// Worktree groups from a root snapshot that this workspace can be launched
/// into. A group qualifies when the daemon resolved it to this workspace —
/// which it only does when every member repo it has on disk belongs here.
function workspaceGroupOptions(
  entries: RootWorktreeEntry[],
  workspaceId: string,
): WorkspaceGroupOption[] {
  const out: WorkspaceGroupOption[] = [];
  for (const entry of entries) {
    const launch = entry.launch;
    if (
      !launch ||
      launch.kind !== "workspace" ||
      launch.workspace_id !== workspaceId
    ) {
      continue;
    }
    const name = launch.branch ?? entry.branch_slug;
    const meta: string[] = [];
    if (entry.status.kind === "active") meta.push("in use");
    if (entry.status.kind === "detached") meta.push("stopped session");
    if (entry.status.kind === "stale") meta.push("stale");
    if (entry.size_bytes != null) meta.push(humanSize(entry.size_bytes));
    if (entry.last_modified_unix != null) {
      meta.push(humanRelativeTime(entry.last_modified_unix));
    }
    out.push({
      key: groupKey(launch.members),
      label: meta.length > 0 ? `${name} — ${meta.join(", ")}` : name,
      branch: launch.branch,
      members: launch.members,
      status: entry.status,
    });
  }
  return out;
}
