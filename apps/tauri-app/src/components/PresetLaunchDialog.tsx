import { useEffect, useMemo, useRef, useState } from "react";
import {
  type DaemonClient,
  launchPreset,
  pickDirectory,
  pickFile,
  previewPreset,
} from "../api";
import type {
  LaunchPresetSource,
  PresetEntry,
  PresetPromptSource,
  PresetTarget,
  PresetVariable,
  RepoEntry,
} from "../types";
import { useEscape } from "../utils/a11y";
import { parsePrompts } from "../utils/parsePrompts";

interface Props {
  preset: PresetEntry;
  target: PresetTarget;
  // Resolves the preset's source repo path so the folder picker can
  // pre-fill an absolute path for repo-relative folder sources.
  repos: RepoEntry[];
  client: DaemonClient;
  onClose: () => void;
}

type SourceKind = "file" | "folder" | "inline";
type Stage = "source" | "variables" | "preview";

const FIRST_LINE_PREVIEW_LIMIT = 80;

export default function PresetLaunchDialog({
  preset,
  target,
  repos,
  client,
  onClose,
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
    const source = buildLaunchSource(sourceKind, {
      filePath,
      folderPath,
      inlineText,
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
    filePath,
    folderPath,
    client,
    target,
    preset.id,
    variableValues,
  ]);

  const promptedVariables = preset.variables.filter((v) => v.prompt_at_launch);

  const canAdvanceFromSource = isSourceReady({
    sourceKind,
    filePath,
    folderPath,
    inlineText,
  });

  const canSubmit =
    stage === "preview" &&
    !previewLoading &&
    previewError === null &&
    previewPrompts.length > 0 &&
    validMaxPerTab(maxPerTab);

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
    });
    if (!source) return;
    const cap = parseMaxPerTab(maxPerTab);
    launchPreset(client, {
      target,
      preset_id: preset.id,
      source,
      variable_values: Object.entries(variableValues),
      use_worktree_override: null,
      max_panes_per_tab_override: cap,
    });
    onClose();
  };

  useEscape(onClose);

  return (
    <div className="modal-backdrop" onClick={onClose} data-testid="preset-launch-dialog">
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
              className={`agent-badge agent-badge-${preset.agent}`}
              title={`Spawns ${preset.agent} sessions`}
            >
              {preset.agent}
            </span>
          </h2>
          <button
            type="button"
            className="link"
            onClick={onClose}
            data-testid="preset-launch-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          {preset.description && (
            <p className="muted" style={{ marginTop: 0 }}>
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
            />
          )}
          {stage === "variables" && (
            <VariablesStage
              variables={promptedVariables}
              values={variableValues}
              onChange={setVariableValues}
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
            />
          )}
        </div>
        <footer className="modal-footer">
          <button type="button" onClick={onClose} data-testid="preset-launch-cancel">
            Cancel
          </button>
          {stage !== "source" && (
            <button type="button" onClick={onBack} data-testid="preset-launch-back">
              Back
            </button>
          )}
          {stage !== "preview" ? (
            <button
              type="button"
              className="primary"
              onClick={onAdvance}
              disabled={!canAdvanceFromSource}
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
              Launch {previewPrompts.length}{" "}
              {previewPrompts.length === 1 ? "session" : "sessions"}
            </button>
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
}) {
  const folderSource = allowed.find((s) => s.kind === "folder");
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
              readOnly
              value={filePath ?? ""}
              placeholder="Pick a .txt or .md file…"
              data-testid="preset-source-file-path"
            />
            <button
              type="button"
              onClick={() =>
                void pickFile({
                  extensions: ["txt", "md"],
                  filterName: "Prompts",
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
              readOnly
              value={folderPath ?? ""}
              placeholder="Pick a folder of .md files…"
              data-testid="preset-source-folder-path"
            />
            <button
              type="button"
              onClick={() =>
                void pickDirectory(folderPath ?? undefined).then(
                  (p) => p && onFolderPathChange(p),
                )
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
    </>
  );
}

function VariablesStage({
  variables,
  values,
  onChange,
}: {
  variables: PresetVariable[];
  values: Record<string, string>;
  onChange: (next: Record<string, string>) => void;
}) {
  const set = (name: string, value: string) =>
    onChange({ ...values, [name]: value });
  return (
    <>
      {variables.map((v) => (
        <label className="field" key={v.name}>
          <span>
            {v.label}
            {!v.optional && <span className="muted"> · required</span>}
          </span>
          <VariableInput
            variable={v}
            value={values[v.name] ?? ""}
            onChange={(val) => set(v.name, val)}
          />
        </label>
      ))}
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
}: {
  variable: PresetVariable;
  value: string;
  onChange: (v: string) => void;
}) {
  const { kind } = variable;
  if (kind.kind === "file_path") {
    return (
      <div className="row">
        <input
          type="text"
          readOnly
          value={value}
          placeholder={
            kind.extensions.length > 0
              ? `Pick a ${kind.extensions.join(" / ")} file…`
              : "Pick a file…"
          }
          data-testid={`preset-variable-${variable.name}`}
        />
        <button
          type="button"
          onClick={() =>
            void pickFile({
              extensions: kind.extensions,
              filterName: variable.label,
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
          readOnly
          value={value}
          placeholder="Pick a folder…"
          data-testid={`preset-variable-${variable.name}`}
        />
        <button
          type="button"
          onClick={() =>
            void pickDirectory(value || undefined).then(
              (p) => p && onChange(p),
            )
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
      onChange={(e) => onChange(e.target.value)}
      placeholder={variable.default ?? ""}
      data-testid={`preset-variable-${variable.name}`}
    />
  );
}

function PreviewStage({
  prompts,
  loading,
  error,
  maxPerTab,
  onMaxPerTabChange,
  defaultCap,
}: {
  prompts: string[];
  loading: boolean;
  error: string | null;
  maxPerTab: string;
  onMaxPerTabChange: (v: string) => void;
  defaultCap: number | null;
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
      <div className="preset-preview-list" data-testid="preset-preview-list">
        {loading || error !== null ? null : prompts.length === 0 ? (
          <p className="muted">No prompts found in the chosen source.</p>
        ) : (
          prompts.map((p, i) => (
            <div key={i} className="preset-preview-row" data-testid="preset-preview-row">
              <span className="preset-preview-index">[{i + 1}]</span>{" "}
              <span>{firstLinePreview(p)}</span>
            </div>
          ))
        )}
      </div>
    </>
  );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function sourceLabel(src: PresetPromptSource): string {
  if (src.kind === "file") return "From file";
  if (src.kind === "folder")
    return `From folder (${src.relative_path})`;
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
  if (base.endsWith("/") || base.endsWith("\\")) return base + rel;
  // Match the base's separator style if obvious; fall back to "/".
  if (base.includes("\\") && !base.includes("/")) return `${base}\\${rel.replace(/\//g, "\\")}`;
  return `${base}/${rel}`;
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
}): boolean {
  if (args.sourceKind === "file") return !!args.filePath;
  if (args.sourceKind === "folder") return !!args.folderPath;
  return parsePrompts(args.inlineText).length > 0;
}

function buildLaunchSource(
  kind: SourceKind,
  args: { filePath: string | null; folderPath: string | null; inlineText: string },
): LaunchPresetSource | null {
  if (kind === "file") {
    return args.filePath ? { kind: "file", path: args.filePath } : null;
  }
  if (kind === "folder") {
    return args.folderPath ? { kind: "folder", path: args.folderPath } : null;
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
