interface Props {
  onSpawn: () => void;
  hasRepos: boolean;
  /// True when this pane lives inside a pop-out window. Pop-outs have no
  /// SpawnDialog of their own, so the "Spawn one here" button would
  /// silently no-op — better to disable it with an explanatory hint.
  inPopout?: boolean;
}

export default function EmptyPane({ onSpawn, hasRepos, inPopout }: Props) {
  return (
    <div className="empty-pane">
      <p className="muted">No session</p>
      {!hasRepos ? (
        <p className="hint">Add a repo from the sidebar first.</p>
      ) : inPopout ? (
        <p className="hint">
          Use the main window to spawn a session, then drag it here.
        </p>
      ) : (
        <button type="button" className="link" onClick={onSpawn}>
          Spawn one here
        </button>
      )}
    </div>
  );
}
