import { useEffect, useMemo, useRef, useState } from "react";
import {
  cancelPresetLaunch,
  type DaemonClient,
  launchPreset,
  pickDirectory,
  pickFile,
  previewPreset,
  resolvePresetScripts,
} from "../api";
import type {
  LaunchPresetSource,
  PresetLaunchJobSnapshot,
  PresetEntry,
  PresetPromptSource,
  PresetTarget,
  PresetVariable,
  RepoEntry,
  ScriptCommandPreview,
} from "../types";
import { useEscape, useFocusReturn } from "../utils/a11y";
import { REMOTE_UNAVAILABLE_TOOLTIP, useIsRemote } from "../utils/remoteMode";
import { parsePrompts } from "../utils/parsePrompts";
import { parseIssueSpec } from "../utils/parseIssueSpec";

interface Props {
  preset: PresetEntry;
  target: PresetTarget;
  // Resolves the preset's source repo path so the folder picker can
  // pre-fill an absolute path for repo-relative folder sources.
  repos: RepoEntry[];
  client: DaemonClient;
  onClose: () => void;
  onSelectSessions: (sessionIds: string[]) => void;
  onStopSessions: (sessionIds: string[]) => void;
}

type SourceKind = "file" | "folder" | "inline" | "github_issues";
type Stage = "source" | "variables" | "preview" | "launching";

const FIRST_LINE_PREVIEW_LIMIT = 80;

export default function PresetLaunchDialog({
  preset,
  target,
  repos,
  client,
  onClose,
  onSelectSessions,
  onStopSessions,
}: Props) {
  const sourceRepo = useMemo(
    () => repos.find((r) => r.id === preset.source_repo_id) ?? null,
    [repos, preset.source_repo_id],
  );

  const allowedSources = preset.prompt_sources;
  const [stage, setStage] = useState<Stage>("source");
  const [sourceKind, setSourceKind] = useState<SourceKind>(
    () => allowedSources[0]?.kind ?? "inline",
  );

  // Source-stage state
  const [filePath, setFilePath] = useState<string | null>(null);
  const [folderPath, setFolderPath] = useState<string | null>(() =>
    defaultFolderPath(allowedSources, sourceRepo),
  );
  const [inlineText, setInlineText] = useState("");
  const [issuesSpec, setIssuesSpec] = useState("");

  // Variables-stage state
  const [variableValues, setVariableValues] = useState<Record<string, string>>(
    () => initialVariableValues(preset.variables),
  );

  // Preview state. For inline sources the client parses synchronously;
  // for file/folder sources the daemon resolves and we track loading/error
  // states. `previewRequestRef` is the latest in-flight request id; older
  // responses are dropped so rapid Back/Next can't race in a stale list.
  const [previewPrompts, setPreviewPrompts] = useState<string[]>([]);
  const [previewLoading, setPreviewLoading] = useState<boolean>(false);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const previewRequestRef = useRef<number>(0);
  const [maxPerTab, setMaxPerTab] = useState<string>(() =>
    defaultMaxPerTabString(preset),
  );

  useEffect(() => {
    if (stage !== "preview") return;
    if (sourceKind === "inline") {
      setPreviewPrompts(parsePrompts(inlineText));
      setPreviewLoading(false);
      setPreviewError(null);
      return;
    }
    if (sourceKind === "github_issues") {
      // Mirrors the daemon's parse_issue_spec so the preview is exact; no
      // round-trip needed for an offline expansion.
      const result = parseIssueSpec(issuesSpec);
      setPreviewLoading(false);
      if (result.ok) {
        setPreviewPrompts(result.issues.map((n) => `GitHub issue #${n}`));
        setPreviewError(null);
      } else {
        setPreviewPrompts([]);
        setPreviewError(result.error);
      }
      return;
    }
    const source = buildLaunchSource(sourceKind, {
      filePath,
      folderPath,
      inlineText,
      issuesSpec,
    });
    if (!source) {
      setPreviewPrompts([]);
      setPreviewLoading(false);
      setPreviewError("source not ready");
      return;
    }
    const ticket = ++previewRequestRef.current;
    setPreviewPrompts([]);
    setPreviewLoading(true);
    setPreviewError(null);
    void previewPreset(client, {
      target,
      preset_id: preset.id,
      source,
      variable_values: Object.entries(variableValues),
    }).then((result) => {
      if (ticket !== previewRequestRef.current) return;
      setPreviewLoading(false);
      if (result.ok) {
        setPreviewPrompts(result.prompts);
        setPreviewError(null);
      } else {
        setPreviewPrompts([]);
        setPreviewError(result.error);
      }
    });
  }, [
    stage,
    sourceKind,
    inlineText,
    issuesSpec,
    filePath,
    folderPath,
    client,
    target,
    preset.id,
    variableValues,
  ]);

  const promptedVariables = preset.variables.filter((v) => v.prompt_at_launch);
  // Script-kind variables produce a process-spawn step gated behind the
  // user clicking Launch. The preview stage renders the rendered commands
  // inline so the user can confirm what executes.
  const scriptVariables = preset.variables.filter(
    (v) => v.kind.kind === "script",
  );
  const sourceRepoPath = sourceRepo?.path ?? null;
  const renderedScriptCommands = useMemo<RenderedScriptCommand[]>(
    () =>
      scriptVariables.map((v) =>
        renderScriptCommand(v, variableValues, sourceRepoPath),
      ),
    [scriptVariables, variableValues, sourceRepoPath],
  );

  // Script-resolution state. When the user clicks Launch (and the preset
  // has script variables), we transition `scriptStage` from "idle" →
  // "running" → "done" / "failed". A failed run lets the user retry from
  // the same Preview stage without losing their inputs.
  type ScriptStage =
    | { kind: "idle" }
    | { kind: "running" }
    | { kind: "failed"; error: string; variableName: string | null }
    | { kind: "done" };
  const [scriptStage, setScriptStage] = useState<ScriptStage>({ kind: "idle" });
  const [launchJobId, setLaunchJobId] = useState<string | null>(null);
  const [launchJob, setLaunchJob] = useState<PresetLaunchJobSnapshot | null>(
    null,
  );

  useEffect(() => {
    if (!launchJobId) return;
    const handler = (ev: Event) => {
      const detail = (
        ev as CustomEvent<{ type: string; job: PresetLaunchJobSnapshot }>
      ).detail;
      if (detail.type !== "preset_launch_job_updated") return;
      if (detail.job.job_id !== launchJobId) return;
      setLaunchJob(detail.job);
    };
    window.addEventListener("rt:preset_launch_job_updated", handler);
    return () => window.removeEventListener("rt:preset_launch_job_updated", handler);
  }, [launchJobId]);

  const canAdvanceFromSource = isSourceReady({
    sourceKind,
    filePath,
    folderPath,
    inlineText,
    issuesSpec,
  });

  // Required variables (prompt_at_launch + !optional) must have a non-empty
  // trimmed value before the Variables → Preview transition fires.
  // Previously a missing required value only surfaced as a daemon-side
  // launch failure — now Next gets disabled and the empty field gets a
  // visual error hint.
  const missingRequiredVariables = promptedVariables.filter(
    (v) =>
      !v.optional &&
      v.kind.kind !== "toggle" &&
      (variableValues[v.name] ?? "").trim().length === 0,
  );
  const canAdvanceFromVariables = missingRequiredVariables.length === 0;

  const canSubmit =
    stage === "preview" &&
    !previewLoading &&
    previewError === null &&
    previewPrompts.length > 0 &&
    validMaxPerTab(maxPerTab) &&
    scriptStage.kind !== "running";

  const onAdvance = () => {
    if (stage === "source") {
      setStage(promptedVariables.length > 0 ? "variables" : "preview");
    } else if (stage === "variables") {
      setStage("preview");
    }
  };

  const onBack = () => {
    if (stage === "variables") setStage("source");
    else if (stage === "preview") {
      setStage(promptedVariables.length > 0 ? "variables" : "source");
    }
  };

  const onLaunch = () => {
    if (!canSubmit) return;
    const source = buildLaunchSource(sourceKind, {
      filePath,
      folderPath,
      inlineText,
      issuesSpec,
    });
    if (!source) return;
    const cap = parseMaxPerTab(maxPerTab);
    // No scripts → fire-and-forget the launch directly.
    if (scriptVariables.length === 0) {
      const jobId = launchPreset(client, {
        target,
        preset_id: preset.id,
        source,
        variable_values: Object.entries(variableValues),
        use_worktree_override: null,
        max_panes_per_tab_override: cap,
      });
      setLaunchJobId(jobId);
      setLaunchJob(null);
      setStage("launching");
      return;
    }
    // Scripts present → resolve first, then launch with the resolved
    // values pre-filled so the daemon doesn't re-spawn.
    setScriptStage({ kind: "running" });
    void resolvePresetScripts(client, {
      target,
      preset_id: preset.id,
      variable_values: Object.entries(variableValues),
    }).then((result) => {
      if (!result.ok) {
        setScriptStage({
          kind: "failed",
          error: result.error,
          variableName: result.variable_name,
        });
        return;
      }
      setScriptStage({ kind: "done" });
      const jobId = launchPreset(client, {
        target,
        preset_id: preset.id,
        source,
        variable_values: result.values,
        use_worktree_override: null,
        max_panes_per_tab_override: cap,
      });
      setLaunchJobId(jobId);
      setLaunchJob(null);
      setStage("launching");
    });
  };

  const onCancelLaunch = () => {
    if (!launchJobId) return;
    cancelPresetLaunch(client, launchJobId);
  };

  useEscape(onClose);
  useFocusReturn();

  return (
    <div className="modal-backdrop" data-testid="preset-launch-dialog">
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label={`Launch preset ${preset.name}`}
      >
        <header className="modal-header">
          <h2>
            Launch preset · {preset.name}{" "}
            <span
              className={`agent-badge agent-badge-${preset.agent_options.kind}`}
              title={`Spawns ${preset.agent_options.kind} sessions`}
            >
              {preset.agent_options.kind}
            </span>
          </h2>
          <button
            type="button"
            className="link"
            onClick={onClose}
            aria-label="Close dialog"
            data-testid="preset-launch-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          {preset.description && (
            <p className="muted preset-description">
              {preset.description}
            </p>
          )}
          {stage === "source" && (
            <SourceStage
              allowed={allowedSources}
              sourceKind={sourceKind}
              onSourceKindChange={setSourceKind}
              filePath={filePath}
              onFilePathChange={setFilePath}
              folderPath={folderPath}
              onFolderPathChange={setFolderPath}
              inlineText={inlineText}
              onInlineTextChange={setInlineText}
              issuesSpec={issuesSpec}
              onIssuesSpecChange={setIssuesSpec}
            />
          )}
          {stage === "variables" && (
            <VariablesStage
              variables={promptedVariables}
              values={variableValues}
              onChange={setVariableValues}
              missingRequired={new Set(missingRequiredVariables.map((v) => v.name))}
            />
          )}
          {stage === "preview" && (
            <PreviewStage
              prompts={previewPrompts}
              loading={previewLoading}
              error={previewError}
              maxPerTab={maxPerTab}
              onMaxPerTabChange={setMaxPerTab}
              defaultCap={defaultCap(preset)}
              scriptCommands={renderedScriptCommands}
              scriptStage={scriptStage}
            />
          )}
          {stage === "launching" && (
            <LaunchProgressStage
              job={launchJob}
              presetName={preset.name}
              onSelectSessions={onSelectSessions}
              onStopSessions={onStopSessions}
            />
          )}
        </div>
        <footer className="modal-footer">
          {stage === "launching" ? (
            <>
              <button
                type="button"
                onClick={onClose}
                data-testid="preset-launch-close-progress"
              >
                Close
              </button>
              <button
                type="button"
                className="danger"
                onClick={onCancelLaunch}
                disabled={!launchJobId || isTerminalLaunchJob(launchJob)}
                data-testid="preset-launch-cancel-job"
              >
                Cancel launch
              </button>
            </>
          ) : (
            <>
              <button
                type="button"
                onClick={onClose}
                disabled={scriptStage.kind === "running"}
                data-testid="preset-launch-cancel"
              >
                Cancel
              </button>
              {stage !== "source" && (
                <button
                  type="button"
                  onClick={onBack}
                  disabled={scriptStage.kind === "running"}
                  data-testid="preset-launch-back"
                >
                  Back
                </button>
              )}
              {stage !== "preview" ? (
                <button
                  type="button"
                  className="primary"
                  onClick={onAdvance}
                  disabled={
                    stage === "source"
                      ? !canAdvanceFromSource
                      : !canAdvanceFromVariables
                  }
                  title={
                    stage === "variables" && !canAdvanceFromVariables
                      ? `Fill in: ${missingRequiredVariables.map((v) => v.label).join(", ")}`
                      : undefined
                  }
                  data-testid="preset-launch-next"
                >
                  Next
                </button>
              ) : (
                <button
                  type="button"
                  className="primary"
                  onClick={onLaunch}
                  disabled={!canSubmit}
                  data-testid="preset-launch-submit"
                >
                  {launchButtonLabel(
                    scriptStage.kind,
                    scriptVariables.length,
                    previewPrompts.length,
                  )}
                </button>
              )}
            </>
          )}
        </footer>
      </div>
    </div>
  );
}

function SourceStage({
  allowed,
  sourceKind,
  onSourceKindChange,
  filePath,
  onFilePathChange,
  folderPath,
  onFolderPathChange,
  inlineText,
  onInlineTextChange,
  issuesSpec,
  onIssuesSpecChange,
}: {
  allowed: PresetPromptSource[];
  sourceKind: SourceKind;
  onSourceKindChange: (s: SourceKind) => void;
  filePath: string | null;
  onFilePathChange: (s: string | null) => void;
  folderPath: string | null;
  onFolderPathChange: (s: string | null) => void;
  inlineText: string;
  onInlineTextChange: (s: string) => void;
  issuesSpec: string;
  onIssuesSpecChange: (s: string) => void;
}) {
  const folderSource = allowed.find((s) => s.kind === "folder");
  const isRemote = useIsRemote();
  return (
    <>
      <fieldset className="field" data-testid="preset-source-stage">
        <legend>Prompt source</legend>
        {allowed.map((src) => (
          <label className="radio" key={src.kind}>
            <input
              type="radio"
              checked={sourceKind === src.kind}
              onChange={() => onSourceKindChange(src.kind)}
              data-testid={`preset-source-kind-${src.kind}`}
            />
            {sourceLabel(src)}
          </label>
        ))}
      </fieldset>
      {sourceKind === "file" && (
        <label className="field">
          <span>Prompts file</span>
          <div className="row">
            <input
              type="text"
              readOnly={!isRemote}
              value={filePath ?? ""}
              onChange={
                isRemote
                  ? (e) => onFilePathChange(e.target.value || null)
                  : undefined
              }
              placeholder={
                isRemote
                  ? "Type a file path on the host…"
                  : "Pick a .txt or .md file…"
              }
              data-testid="preset-source-file-path"
            />
            <button
              type="button"
              disabled={isRemote}
              title={isRemote ? REMOTE_UNAVAILABLE_TOOLTIP : undefined}
              onClick={() =>
                void pickFile({
                  extensions: ["txt", "md"],
                  filterName: "Prompts",
                  lastDirKey: "presetSourceFile",
                }).then((p) => p && onFilePathChange(p))
              }
              data-testid="preset-source-file-pick"
            >
              Pick…
            </button>
          </div>
        </label>
      )}
      {sourceKind === "folder" && (
        <label className="field">
          <span>
            Prompts folder
            {folderSource && folderSource.kind === "folder" && (
              <span className="muted">
                {" "}
                · default: <code>{folderSource.relative_path}</code>
              </span>
            )}
          </span>
          <div className="row">
            <input
              type="text"
              readOnly={!isRemote}
              value={folderPath ?? ""}
              onChange={
                isRemote
                  ? (e) => onFolderPathChange(e.target.value || null)
                  : undefined
              }
              placeholder={
                isRemote
                  ? "Type a folder path on the host…"
                  : "Pick a folder of .md files…"
              }
              data-testid="preset-source-folder-path"
            />
            <button
              type="button"
              disabled={isRemote}
              title={isRemote ? REMOTE_UNAVAILABLE_TOOLTIP : undefined}
              onClick={() =>
                void pickDirectory(folderPath ?? undefined, {
                  lastDirKey: "presetSourceFolder",
                }).then((p) => p && onFolderPathChange(p))
              }
              data-testid="preset-source-folder-pick"
            >
              Pick…
            </button>
          </div>
        </label>
      )}
      {sourceKind === "inline" && (
        <label className="field">
          <span>Prompts (one per bullet or paragraph)</span>
          <textarea
            rows={8}
            value={inlineText}
            onChange={(e) => onInlineTextChange(e.target.value)}
            placeholder={
              "- first prompt\n- second prompt\n\n" +
              "or paragraph-separated:\n\nfirst prompt\nstill first\n\nsecond prompt"
            }
            data-testid="preset-source-inline-text"
          />
        </label>
      )}
      {sourceKind === "github_issues" && (
        <GithubIssuesField spec={issuesSpec} onChange={onIssuesSpecChange} />
      )}
    </>
  );
}

function GithubIssuesField({
  spec,
  onChange,
}: {
  spec: string;
  onChange: (s: string) => void;
}) {
  const trimmed = spec.trim();
  const result = trimmed === "" ? null : parseIssueSpec(spec);
  return (
    <label className="field">
      <span>
        GitHub issues{" "}
        <span className="muted">· one session per issue</span>
      </span>
      <input
        type="text"
        value={spec}
        onChange={(e) => onChange(e.target.value)}
        placeholder="123-127, 131, 134-136"
        data-testid="preset-source-github-issues-spec"
      />
      {result === null ? (
        <span className="muted small">
          Comma-separated issue numbers; ranges like <code>123-127</code> are
          inclusive.
        </span>
      ) : result.ok ? (
        <span className="muted small" data-testid="preset-source-github-issues-ok">
          {result.issues.length}{" "}
          {result.issues.length === 1 ? "issue" : "issues"} →{" "}
          {result.issues.length === 1 ? "session" : "sessions"}:{" "}
          {previewIssueList(result.issues)}
        </span>
      ) : (
        <span
          className="error small"
          data-testid="preset-source-github-issues-error"
        >
          {result.error}
        </span>
      )}
    </label>
  );
}

function previewIssueList(issues: number[]): string {
  const head = issues.slice(0, 12).map((n) => `#${n}`);
  return issues.length > head.length
    ? `${head.join(", ")}, … (+${issues.length - head.length})`
    : head.join(", ");
}

function VariablesStage({
  variables,
  values,
  onChange,
  missingRequired,
}: {
  variables: PresetVariable[];
  values: Record<string, string>;
  onChange: (next: Record<string, string>) => void;
  missingRequired: Set<string>;
}) {
  const set = (name: string, value: string) =>
    onChange({ ...values, [name]: value });
  return (
    <>
      {variables.map((v) => {
        const isMissing = missingRequired.has(v.name);
        return (
          <label className="field" key={v.name}>
            <span>
              {v.label}
              {!v.optional && <span className="muted"> · required</span>}
            </span>
            <VariableInput
              variable={v}
              value={values[v.name] ?? ""}
              onChange={(val) => set(v.name, val)}
              invalid={isMissing}
            />
            {isMissing && (
              <span className="muted small env-row-error">
                Required — fill in before continuing.
              </span>
            )}
          </label>
        );
      })}
      {variables.length === 0 && (
        <p className="muted">No variables to configure.</p>
      )}
    </>
  );
}

function VariableInput({
  variable,
  value,
  onChange,
  invalid,
}: {
  variable: PresetVariable;
  value: string;
  onChange: (v: string) => void;
  invalid: boolean;
}) {
  const invalidClass = invalid ? "input-invalid" : "";
  const isRemote = useIsRemote();
  const { kind } = variable;
  if (kind.kind === "toggle") {
    // The enclosing field `<label>` already carries the variable label and
    // makes the text a click target, so render just the checkbox here.
    return (
      <input
        type="checkbox"
        className="preset-variable-toggle"
        checked={value === "true"}
        onChange={(e) => onChange(e.target.checked ? "true" : "false")}
        data-testid={`preset-variable-${variable.name}`}
      />
    );
  }
  if (kind.kind === "file_path") {
    return (
      <div className="row">
        <input
          type="text"
          readOnly={!isRemote}
          value={value}
          onChange={isRemote ? (e) => onChange(e.target.value) : undefined}
          className={invalidClass}
          aria-invalid={invalid}
          placeholder={
            isRemote
              ? "Type a file path on the host…"
              : kind.extensions.length > 0
                ? `Pick a ${kind.extensions.join(" / ")} file…`
                : "Pick a file…"
          }
          data-testid={`preset-variable-${variable.name}`}
        />
        <button
          type="button"
          disabled={isRemote}
          title={isRemote ? REMOTE_UNAVAILABLE_TOOLTIP : undefined}
          onClick={() =>
            void pickFile({
              extensions: kind.extensions,
              filterName: variable.label,
              lastDirKey: `presetVar:${variable.name}`,
            }).then((p) => p && onChange(p))
          }
          data-testid={`preset-variable-${variable.name}-pick`}
        >
          Pick…
        </button>
        {value !== "" && (
          <button
            type="button"
            onClick={() => onChange("")}
            data-testid={`preset-variable-${variable.name}-clear`}
          >
            Clear
          </button>
        )}
      </div>
    );
  }
  if (kind.kind === "folder_path") {
    return (
      <div className="row">
        <input
          type="text"
          readOnly={!isRemote}
          value={value}
          onChange={isRemote ? (e) => onChange(e.target.value) : undefined}
          className={invalidClass}
          aria-invalid={invalid}
          placeholder={
            isRemote ? "Type a folder path on the host…" : "Pick a folder…"
          }
          data-testid={`preset-variable-${variable.name}`}
        />
        <button
          type="button"
          disabled={isRemote}
          title={isRemote ? REMOTE_UNAVAILABLE_TOOLTIP : undefined}
          onClick={() =>
            void pickDirectory(value || undefined, {
              lastDirKey: `presetVar:${variable.name}`,
            }).then((p) => p && onChange(p))
          }
          data-testid={`preset-variable-${variable.name}-pick`}
        >
          Pick…
        </button>
        {value !== "" && (
          <button
            type="button"
            onClick={() => onChange("")}
            data-testid={`preset-variable-${variable.name}-clear`}
          >
            Clear
          </button>
        )}
      </div>
    );
  }
  // Text fallback (also for env_var / literal_path overrides when the user
  // really wants to type a different value).
  return (
    <input
      type="text"
      value={value}
      className={invalidClass}
      aria-invalid={invalid}
      onChange={(e) => onChange(e.target.value)}
      placeholder={variable.default ?? ""}
      data-testid={`preset-variable-${variable.name}`}
    />
  );
}

type RenderedScriptCommand = {
  variable: PresetVariable;
} & (
  | { state: "ready"; command: ScriptCommandPreview }
  | { state: "skipped"; guard: string }
  | { state: "unresolved" }
);

type ScriptStage =
  | { kind: "idle" }
  | { kind: "running" }
  | { kind: "failed"; error: string; variableName: string | null }
  | { kind: "done" };

function PreviewStage({
  prompts,
  loading,
  error,
  maxPerTab,
  onMaxPerTabChange,
  defaultCap,
  scriptCommands,
  scriptStage,
}: {
  prompts: string[];
  loading: boolean;
  error: string | null;
  maxPerTab: string;
  onMaxPerTabChange: (v: string) => void;
  defaultCap: number | null;
  scriptCommands: RenderedScriptCommand[];
  scriptStage: ScriptStage;
}) {
  const tabCount = computeTabCount(prompts.length, maxPerTab);
  return (
    <>
      {loading ? (
        <p className="muted" data-testid="preset-preview-loading">
          Resolving prompts…
        </p>
      ) : error !== null ? (
        <p className="error" data-testid="preset-preview-error">
          Could not resolve prompts: {error}
        </p>
      ) : (
        <p className="muted">
          {prompts.length} {prompts.length === 1 ? "prompt" : "prompts"} ready
          to spawn.
          {tabCount !== null && (
            <>
              {" "}
              → {tabCount} {tabCount === 1 ? "tab" : "tabs"}.
            </>
          )}
        </p>
      )}
      {scriptCommands.length > 0 && (
        <ScriptCommandsSection
          commands={scriptCommands}
          stage={scriptStage}
        />
      )}
      <label className="field">
        <span>
          Max panes per tab
          <span className="muted">
            {" "}
            · default {defaultCap === null ? "unlimited" : defaultCap}
          </span>
        </span>
        <input
          type="number"
          min={1}
          step={1}
          value={maxPerTab}
          onChange={(e) => onMaxPerTabChange(e.target.value)}
          placeholder="(leave blank for the preset default)"
          data-testid="preset-max-per-tab"
        />
      </label>
      {!loading && error === null && prompts.length > 0 && (
        <p
          className="muted small preset-launch-caveat"
          data-testid="preset-launch-caveat"
        >
          After launch starts, this dialog shows progress and can cancel before
          the next session spawns. Already-created sessions remain visible.
        </p>
      )}
      <div className="preset-preview-list" data-testid="preset-preview-list">
        {loading || error !== null ? null : prompts.length === 0 ? (
          <p className="muted">No prompts found in the chosen source.</p>
        ) : (
          <PreviewPromptList
            prompts={prompts}
            cap={parseMaxPerTab(maxPerTab)}
          />
        )}
      </div>
    </>
  );
}

function LaunchProgressStage({
  job,
  presetName,
  onSelectSessions,
  onStopSessions,
}: {
  job: PresetLaunchJobSnapshot | null;
  presetName: string;
  onSelectSessions: (sessionIds: string[]) => void;
  onStopSessions: (sessionIds: string[]) => void;
}) {
  const status = launchJobStatusText(job);
  const total = job?.total ?? 0;
  const launched = job?.launched ?? 0;
  const createdSessionIds = job?.created_session_ids ?? [];
  const canManageCreatedSessions =
    job !== null && isTerminalLaunchJob(job) && createdSessionIds.length > 0;
  const progressMax = Math.max(1, total);
  const progressValue = Math.min(progressMax, launched);
  return (
    <div className="preset-launch-progress" data-testid="preset-launch-progress">
      <p className="muted small">Preset</p>
      <h3>{presetName}</h3>
      <p data-testid="preset-launch-progress-status">{status}</p>
      <progress
        className="preset-launch-progress-bar"
        max={progressMax}
        value={progressValue}
        aria-label="Preset launch progress"
        data-testid="preset-launch-progress-bar"
      />
      {job && (
        <dl className="preset-launch-progress-facts">
          <div>
            <dt>Sessions</dt>
            <dd data-testid="preset-launch-progress-sessions">
              {job.created_session_ids.length}
            </dd>
          </div>
          <div>
            <dt>Tabs</dt>
            <dd>{job.created_tab_ids.length}</dd>
          </div>
        </dl>
      )}
      {job?.status === "failed" && job.error && (
        <p className="error" data-testid="preset-launch-progress-error">
          {job.error}
        </p>
      )}
      {canManageCreatedSessions && (
        <div
          className="preset-launch-cleanup"
          data-testid="preset-launch-cleanup"
        >
          <p>
            {createdSessionIds.length} launched{" "}
            {createdSessionIds.length === 1 ? "session is" : "sessions are"}{" "}
            still visible in the sidebar.
          </p>
          <div className="preset-launch-cleanup-actions">
            <button
              type="button"
              onClick={() => onSelectSessions(createdSessionIds)}
              data-testid="preset-launch-select-created"
            >
              Select launched sessions
            </button>
            <button
              type="button"
              className="danger"
              onClick={() => onStopSessions(createdSessionIds)}
              data-testid="preset-launch-stop-created"
            >
              Stop all from this preset
            </button>
            <span className="muted small">Close to leave them running.</span>
          </div>
        </div>
      )}
    </div>
  );
}

function launchJobStatusText(job: PresetLaunchJobSnapshot | null): string {
  if (!job) {
    return "Starting launch…";
  }
  switch (job.status) {
    case "resolving":
      return "Resolving prompts…";
    case "running_scripts":
      return "Running script commands…";
    case "spawning":
      return `Spawning ${job.launched} / ${job.total} sessions`;
    case "completed":
      return `Launched ${job.launched} / ${job.total} sessions`;
    case "cancelled":
      return `Cancelled after ${job.launched} / ${job.total} sessions`;
    case "failed":
      return `Failed after ${job.launched} / ${job.total} sessions`;
    case "unknown":
      return `Launch status: ${job.status}`;
  }
}

function isTerminalLaunchJob(job: PresetLaunchJobSnapshot | null): boolean {
  return (
    job?.status === "completed" ||
    job?.status === "cancelled" ||
    job?.status === "failed"
  );
}

/// Renders the preview list either flat (single-tab case) or grouped by
/// destination tab with a small `Tab N of M` header above each chunk.
/// Pre-iter-47 the user only saw "→ 3 tabs" in the prose line above and
/// had no idea which prompts would land in which tab. The grouping uses
/// the same sequential `prompts.slice(i, i + cap)` partition the daemon
/// applies in `presets.rs::launch` (single-tab path falls through cleanly
/// when `cap === null` or `prompts.length <= cap`).
function PreviewPromptList({
  prompts,
  cap,
}: {
  prompts: string[];
  cap: number | null;
}) {
  if (cap === null || prompts.length <= cap) {
    return (
      <>
        {prompts.map((p, i) => (
          <div
            key={i}
            className="preset-preview-row"
            data-testid="preset-preview-row"
          >
            <span className="preset-preview-index">[{i + 1}]</span>{" "}
            <span>{firstLinePreview(p)}</span>
          </div>
        ))}
      </>
    );
  }
  const groups: Array<{ start: number; items: string[] }> = [];
  for (let i = 0; i < prompts.length; i += cap) {
    groups.push({ start: i, items: prompts.slice(i, i + cap) });
  }
  return (
    <>
      {groups.map((g, gi) => (
        <div
          key={gi}
          className="preset-preview-group"
          data-testid="preset-preview-group"
        >
          <div className="preset-preview-group-header">
            Tab {gi + 1} of {groups.length}
          </div>
          {g.items.map((p, j) => {
            const idx = g.start + j;
            return (
              <div
                key={idx}
                className="preset-preview-row"
                data-testid="preset-preview-row"
              >
                <span className="preset-preview-index">[{idx + 1}]</span>{" "}
                <span>{firstLinePreview(p)}</span>
              </div>
            );
          })}
        </div>
      ))}
    </>
  );
}

function ScriptCommandsSection({
  commands,
  stage,
}: {
  commands: RenderedScriptCommand[];
  stage: ScriptStage;
}) {
  const failedVar =
    stage.kind === "failed" ? stage.variableName : null;
  return (
    <fieldset
      className="field preset-script-section"
      data-testid="preset-script-section"
    >
      <legend>
        Scripts to run
        {stage.kind === "running" && (
          <span className="muted"> · running…</span>
        )}
        {stage.kind === "done" && <span className="muted"> · done</span>}
      </legend>
      <p className="muted small">
        These commands run on the daemon before any session spawns. They
        execute with the repo as their working directory.
      </p>
      {commands.map((entry) => (
        <div
          key={entry.variable.name}
          className={`preset-script-row${failedVar === entry.variable.name ? " preset-script-row-failed" : ""}`}
          data-testid={`preset-script-row-${entry.variable.name}`}
        >
          <div className="preset-script-label">
            {entry.variable.label}
            {entry.state === "skipped" && (
              <span className="muted"> · skipped ({entry.guard} empty)</span>
            )}
          </div>
          {entry.state === "ready" ? (
            <code className="preset-script-cmd">
              {[entry.command.cmd, ...entry.command.args].join(" ")}
            </code>
          ) : entry.state === "unresolved" ? (
            <span className="error small">
              Unable to render command — fix earlier variable inputs.
            </span>
          ) : null}
        </div>
      ))}
      {stage.kind === "failed" && (
        <p
          className="error small"
          data-testid="preset-script-error"
        >
          Script resolution failed: {stage.error}
        </p>
      )}
    </fieldset>
  );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function sourceLabel(src: PresetPromptSource): string {
  if (src.kind === "file") return "From file";
  if (src.kind === "folder")
    return `From folder (${src.relative_path})`;
  if (src.kind === "github_issues") return "From GitHub issues";
  return "Inline";
}

function defaultFolderPath(
  allowed: PresetPromptSource[],
  repo: RepoEntry | null,
): string | null {
  const folderSource = allowed.find((s) => s.kind === "folder");
  if (!folderSource || folderSource.kind !== "folder" || !repo) return null;
  return joinPath(repo.path, folderSource.relative_path);
}

function joinPath(base: string, rel: string): string {
  // The repo path comes from a Tauri file picker (native separators), but
  // the preset's `relative_path` is whatever the preset author wrote — so
  // the two halves often disagree on slash direction. Detect Windows from
  // the base shape (drive letter `X:` or UNC `\\server`), normalise the
  // relative half to match, and strip stray inner separators so we don't
  // emit `C:\foo\\bar` or `C:\foo/bar` on either platform.
  const isWindows = /^[A-Za-z]:/.test(base) || base.startsWith("\\\\");
  const sep = isWindows ? "\\" : "/";
  const trimmedBase = base.replace(/[/\\]+$/, "");
  const trimmedRel = rel.replace(/^[/\\]+/, "");
  const normalisedRel = isWindows
    ? trimmedRel.replace(/\//g, "\\")
    : trimmedRel.replace(/\\/g, "/");
  return trimmedRel === ""
    ? trimmedBase
    : `${trimmedBase}${sep}${normalisedRel}`;
}

function initialVariableValues(
  variables: PresetVariable[],
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const v of variables) {
    if (v.default !== null) out[v.name] = v.default;
  }
  return out;
}

function isSourceReady(args: {
  sourceKind: SourceKind;
  filePath: string | null;
  folderPath: string | null;
  inlineText: string;
  issuesSpec: string;
}): boolean {
  if (args.sourceKind === "file") return !!args.filePath;
  if (args.sourceKind === "folder") return !!args.folderPath;
  if (args.sourceKind === "github_issues")
    return parseIssueSpec(args.issuesSpec).ok;
  return parsePrompts(args.inlineText).length > 0;
}

function buildLaunchSource(
  kind: SourceKind,
  args: {
    filePath: string | null;
    folderPath: string | null;
    inlineText: string;
    issuesSpec: string;
  },
): LaunchPresetSource | null {
  if (kind === "file") {
    return args.filePath ? { kind: "file", path: args.filePath } : null;
  }
  if (kind === "folder") {
    return args.folderPath ? { kind: "folder", path: args.folderPath } : null;
  }
  if (kind === "github_issues") {
    return parseIssueSpec(args.issuesSpec).ok
      ? { kind: "github_issues", spec: args.issuesSpec }
      : null;
  }
  return { kind: "inline", prompts: parsePrompts(args.inlineText) };
}

function firstLinePreview(prompt: string): string {
  const first = prompt.split("\n")[0] ?? "";
  if (first.length <= FIRST_LINE_PREVIEW_LIMIT) return first;
  return `${first.slice(0, FIRST_LINE_PREVIEW_LIMIT - 3)}...`;
}

function defaultCap(preset: PresetEntry): number | null {
  if (preset.tab_grouping.kind !== "new_tab") return null;
  return preset.tab_grouping.max_panes_per_tab;
}

function defaultMaxPerTabString(preset: PresetEntry): string {
  const cap = defaultCap(preset);
  return cap === null ? "" : String(cap);
}

function validMaxPerTab(value: string): boolean {
  if (value.trim() === "") return true;
  const n = Number.parseInt(value, 10);
  return Number.isInteger(n) && n >= 1;
}

function parseMaxPerTab(value: string): number | null {
  if (value.trim() === "") return null;
  const n = Number.parseInt(value, 10);
  return Number.isInteger(n) && n >= 1 ? n : null;
}

function computeTabCount(promptCount: number, maxPerTab: string): number | null {
  if (promptCount === 0) return null;
  const cap = parseMaxPerTab(maxPerTab);
  if (cap === null) return 1;
  return Math.ceil(promptCount / cap);
}

/// Render a Script variable's `cmd` + `args` against the user's current
/// variable inputs. Returns a `skipped` marker when `skip_if_empty`'s
/// guard is empty, an `unresolved` marker when interpolation hits an
/// undefined `{var_name}`, and `ready` with the rendered command
/// otherwise. The dialog renders all three states so the user sees what
/// would (or wouldn't) run.
function renderScriptCommand(
  v: PresetVariable,
  values: Record<string, string>,
  repoRoot: string | null,
): RenderedScriptCommand {
  if (v.kind.kind !== "script") {
    return { variable: v, state: "unresolved" };
  }
  if (v.kind.skip_if_empty !== null) {
    const guard = v.kind.skip_if_empty;
    const guardVal = (values[guard] ?? "").trim();
    if (guardVal === "") {
      return { variable: v, state: "skipped", guard };
    }
  }
  const rendered: string[] = [];
  for (const a of v.kind.args) {
    const out = interpolateScriptToken(a, values, repoRoot);
    if (out === null) {
      return { variable: v, state: "unresolved" };
    }
    rendered.push(out);
  }
  return {
    variable: v,
    state: "ready",
    command: { variable_name: v.name, cmd: v.kind.cmd, args: rendered },
  };
}

function interpolateScriptToken(
  raw: string,
  values: Record<string, string>,
  repoRoot: string | null,
): string | null {
  let out = "";
  let rest = raw;
  while (true) {
    const start = rest.indexOf("{");
    if (start < 0) {
      out += rest;
      return out;
    }
    out += rest.slice(0, start);
    const after = rest.slice(start + 1);
    const end = after.indexOf("}");
    if (end < 0) {
      out += rest.slice(start);
      return out;
    }
    const name = after.slice(0, end);
    if (name === "repo_root") {
      if (repoRoot === null) return null;
      out += repoRoot;
    } else if (Object.prototype.hasOwnProperty.call(values, name)) {
      out += values[name] ?? "";
    } else {
      // Unknown placeholder — daemon would reject it; flag here so the
      // preview accurately mirrors what would execute.
      return null;
    }
    rest = after.slice(end + 1);
  }
}

function launchButtonLabel(
  scriptStage: ScriptStage["kind"],
  scriptCount: number,
  promptCount: number,
): string {
  const sessions = `${promptCount} ${promptCount === 1 ? "session" : "sessions"}`;
  if (scriptStage === "running") return "Running scripts…";
  if (scriptStage === "failed") {
    return scriptCount === 1
      ? `Retry script & launch ${sessions}`
      : `Retry scripts & launch ${sessions}`;
  }
  if (scriptCount === 0) return `Launch ${sessions}`;
  return scriptCount === 1
    ? `Run script & launch ${sessions}`
    : `Run scripts & launch ${sessions}`;
}
