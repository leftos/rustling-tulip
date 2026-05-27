/**
 * Renders a Monaco DiffEditor showing the OLD and NEW file content for a
 * (repo, path, against) triple. Drives the lifecycle:
 *
 *   1. On mount (and on prop change), send `get_file_snapshot` to the
 *      daemon with a fresh request id.
 *   2. Listen for `rt:file_snapshot` events; ignore those whose id
 *      doesn't match (stale responses from rapid tab switching).
 *   3. Once content arrives, construct two `monaco.editor.ITextModel`s
 *      with the daemon-provided language and feed them to a single
 *      DiffEditor instance.
 *
 * The editor is read-only (Phase B does not allow edits from inside the
 * diff view) and themed against the surrounding dark UI. Models are
 * disposed when the component unmounts or the props change, since
 * Monaco won't garbage-collect them automatically.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import * as monaco from "monaco-editor";
import type { DaemonClient } from "../api";
import type { DaemonMessage } from "../types";

interface Props {
  client: DaemonClient;
  repoId: string;
  path: string;
  against: string | null;
}

interface Snapshot {
  old: string;
  new: string;
  language: string;
}

export default function DiffPane({ client, repoId, path, against }: Props) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const editorRef = useRef<monaco.editor.IStandaloneDiffEditor | null>(null);
  const modelsRef = useRef<{
    original: monaco.editor.ITextModel;
    modified: monaco.editor.ITextModel;
  } | null>(null);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Live count of diff hunks Monaco has computed for the current models.
  // `null` while no diff has been reported yet (models still loading); `0`
  // once Monaco has reported "no changes". Used to disable nav buttons
  // and surface the change count to the user.
  const [diffCount, setDiffCount] = useState<number | null>(null);

  // Fetch snapshot from the daemon.
  useEffect(() => {
    setSnapshot(null);
    setError(null);
    const reqId = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    let cancelled = false;
    const handleOk = (ev: Event) => {
      if (cancelled) return;
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "file_snapshot" || detail.id !== reqId) return;
      setSnapshot({
        old: detail.old,
        new: detail.new,
        language: detail.language,
      });
    };
    const handleErr = (ev: Event) => {
      if (cancelled) return;
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "file_snapshot_error" || detail.id !== reqId) return;
      setError(detail.error);
    };
    window.addEventListener("rt:file_snapshot", handleOk);
    window.addEventListener("rt:file_snapshot_error", handleErr);
    client.send({
      type: "get_file_snapshot",
      id: reqId,
      repo_id: repoId,
      path,
      against,
    });
    return () => {
      cancelled = true;
      window.removeEventListener("rt:file_snapshot", handleOk);
      window.removeEventListener("rt:file_snapshot_error", handleErr);
    };
  }, [client, repoId, path, against]);

  // Construct the editor once the host element is mounted.
  useEffect(() => {
    if (!hostRef.current) return;
    const editor = monaco.editor.createDiffEditor(hostRef.current, {
      theme: "vs-dark",
      readOnly: true,
      automaticLayout: true,
      renderSideBySide: true,
      renderOverviewRuler: false,
      scrollBeyondLastLine: false,
      ignoreTrimWhitespace: false,
      fontSize: 12,
    });
    editorRef.current = editor;
    return () => {
      editor.dispose();
      editorRef.current = null;
      if (modelsRef.current) {
        modelsRef.current.original.dispose();
        modelsRef.current.modified.dispose();
        modelsRef.current = null;
      }
    };
  }, []);

  // Sync content into the editor whenever the snapshot changes.
  useEffect(() => {
    const editor = editorRef.current;
    if (!editor || !snapshot) return;
    if (modelsRef.current) {
      modelsRef.current.original.dispose();
      modelsRef.current.modified.dispose();
    }
    const original = monaco.editor.createModel(snapshot.old, snapshot.language);
    const modified = monaco.editor.createModel(snapshot.new, snapshot.language);
    modelsRef.current = { original, modified };
    setDiffCount(null);
    editor.setModel({ original, modified });
  }, [snapshot]);

  // Keep `diffCount` in sync with Monaco's computed diff so the nav
  // buttons enable as soon as the diff is ready and accurately reflect
  // mid-session updates (e.g. an ignoreTrimWhitespace toggle in the
  // future). `onDidUpdateDiff` is the canonical signal that the diff
  // computation finished for the current model pair.
  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    const disposable = editor.onDidUpdateDiff(() => {
      const changes = editor.getLineChanges() ?? [];
      setDiffCount(changes.length);
    });
    return () => disposable.dispose();
  }, [snapshot]);

  /// Jump to a given diff hunk and place the cursor on its first changed
  /// line in the modified editor. `idx` is clamped to the valid range.
  const goToDiff = useCallback((idx: number) => {
    const editor = editorRef.current;
    if (!editor) return;
    const changes = editor.getLineChanges();
    if (!changes || changes.length === 0) return;
    const clamped = Math.max(0, Math.min(idx, changes.length - 1));
    const change = changes[clamped];
    if (!change) return;
    const modified = editor.getModifiedEditor();
    // `modifiedStartLineNumber` is 0 when the change is a pure deletion
    // (no modified-side line). Fall back to the next line so we still
    // reveal something meaningful.
    const line = Math.max(1, change.modifiedStartLineNumber || 1);
    modified.revealLineInCenter(line);
    modified.setPosition({ lineNumber: line, column: 1 });
    modified.focus();
  }, []);

  const goToPrevDiff = useCallback(() => {
    const editor = editorRef.current;
    if (!editor) return;
    const changes = editor.getLineChanges();
    if (!changes || changes.length === 0) return;
    const cur = editor.getModifiedEditor().getPosition()?.lineNumber ?? 0;
    // Scan from the end so we pick the LAST change whose start line is
    // strictly less than the cursor (the immediate predecessor). If none,
    // wrap to the last change.
    let target = changes.length - 1;
    for (let i = changes.length - 1; i >= 0; i -= 1) {
      const c = changes[i];
      if (c && (c.modifiedStartLineNumber || 1) < cur) {
        target = i;
        break;
      }
    }
    goToDiff(target);
  }, [goToDiff]);

  const goToNextDiff = useCallback(() => {
    const editor = editorRef.current;
    if (!editor) return;
    const changes = editor.getLineChanges();
    if (!changes || changes.length === 0) return;
    const cur = editor.getModifiedEditor().getPosition()?.lineNumber ?? 0;
    const idx = changes.findIndex(
      (c) => (c.modifiedStartLineNumber || 1) > cur,
    );
    // If no change starts past the cursor, wrap to the first.
    goToDiff(idx === -1 ? 0 : idx);
  }, [goToDiff]);

  const hasDiffs = diffCount !== null && diffCount > 0;

  if (error) {
    return (
      <div className="diff-pane-host" data-testid="diff-pane-error">
        <p className="empty">
          Could not load diff: {error}
        </p>
      </div>
    );
  }
  return (
    <div className="diff-pane-host" data-testid="diff-pane">
      <header className="diff-pane-header">
        <span className="path" title={path}>
          {path}
        </span>
        <span className="muted small">
          {against === null ? "worktree vs index" : `vs ${against}`}
        </span>
        <div
          className="diff-pane-nav"
          data-testid="diff-pane-nav"
          role="toolbar"
          aria-label="Diff navigation"
        >
          <span
            className="muted small diff-pane-nav-count"
            data-testid="diff-pane-nav-count"
            aria-live="polite"
          >
            {diffCount === null
              ? "…"
              : diffCount === 0
                ? "no diffs"
                : `${diffCount} diff${diffCount === 1 ? "" : "s"}`}
          </span>
          <button
            type="button"
            className="diff-pane-nav-button"
            onClick={() => goToDiff(0)}
            disabled={!hasDiffs}
            aria-label="Go to first diff"
            title="Go to first diff"
            data-testid="diff-pane-nav-first"
          >
            ⏮
          </button>
          <button
            type="button"
            className="diff-pane-nav-button"
            onClick={goToPrevDiff}
            disabled={!hasDiffs}
            aria-label="Go to previous diff"
            title="Go to previous diff"
            data-testid="diff-pane-nav-prev"
          >
            ◀
          </button>
          <button
            type="button"
            className="diff-pane-nav-button"
            onClick={goToNextDiff}
            disabled={!hasDiffs}
            aria-label="Go to next diff"
            title="Go to next diff"
            data-testid="diff-pane-nav-next"
          >
            ▶
          </button>
          <button
            type="button"
            className="diff-pane-nav-button"
            onClick={() => goToDiff(Number.MAX_SAFE_INTEGER)}
            disabled={!hasDiffs}
            aria-label="Go to last diff"
            title="Go to last diff"
            data-testid="diff-pane-nav-last"
          >
            ⏭
          </button>
        </div>
      </header>
      <div
        ref={hostRef}
        className="diff-pane-editor"
        data-testid="diff-pane-editor"
      />
      {!snapshot && (
        <div
          className="diff-pane-loading"
          data-testid="diff-pane-loading"
        >
          loading editor…
        </div>
      )}
    </div>
  );
}
