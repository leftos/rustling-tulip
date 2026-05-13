import Icon from "./Icon";

export interface UndoShelfEntry {
  id: string;
  message: string;
  actionLabel: string;
  expiresAt: number;
}

interface Props {
  entries: UndoShelfEntry[];
  onUndo: (id: string) => void;
  onDismiss: (id: string) => void;
}

export default function UndoShelf({ entries, onUndo, onDismiss }: Props) {
  if (entries.length === 0) {
    return null;
  }

  return (
    <div className="undo-shelf" data-testid="undo-shelf" aria-live="polite">
      {entries.map((entry) => (
        <div className="undo-entry" data-testid="undo-entry" key={entry.id}>
          <span className="undo-message">{entry.message}</span>
          <button
            type="button"
            className="undo-action"
            onClick={() => onUndo(entry.id)}
            data-testid="undo-action"
          >
            {entry.actionLabel}
          </button>
          <button
            type="button"
            className="undo-dismiss"
            onClick={() => onDismiss(entry.id)}
            aria-label="Dismiss undo"
            title="Dismiss undo"
            data-testid="undo-dismiss"
          >
            <Icon name="close" />
          </button>
        </div>
      ))}
    </div>
  );
}
