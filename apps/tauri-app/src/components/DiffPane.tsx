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
import { useEffect, useRef, useState } from "react";
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
    editor.setModel({ original, modified });
  }, [snapshot]);

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
