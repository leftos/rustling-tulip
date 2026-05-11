/**
 * Monaco worker bootstrap. Imported once from `main.tsx` so the worker
 * URLs are registered before any `<DiffEditor />` mounts.
 *
 * We only ship the generic editor worker — that's enough for syntax
 * highlighting via Monarch (Monaco's tokenizer runs in the main thread
 * for most languages). Language-service workers (TypeScript IntelliSense,
 * JSON schema validation, CSS linting) are intentionally not bundled:
 * we render diffs read-only and don't need completions or diagnostics.
 */
import EditorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";

self.MonacoEnvironment = {
  getWorker: (_workerId: string, _label: string) => new EditorWorker(),
};
