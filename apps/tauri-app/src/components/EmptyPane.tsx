interface Props {
  onSpawn: () => void;
  hasRepos: boolean;
}

export default function EmptyPane({ onSpawn, hasRepos }: Props) {
  return (
    <div className="empty-pane">
      <p className="muted">No session</p>
      {hasRepos ? (
        <button type="button" className="link" onClick={onSpawn}>
          Spawn one here
        </button>
      ) : (
        <p className="hint">Add a repo from the sidebar first.</p>
      )}
    </div>
  );
}
