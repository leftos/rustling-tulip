import {
  useCallback,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import { recoverSessions, type DaemonClient } from "../api";
import type {
  RecoverItem,
  RepoEntry,
  SessionHistoryItem,
  WorkspaceEntry,
} from "../types";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";
import {
  candidateLabel,
  defaultConversations,
  disabledReason,
  endedText,
  groupHistory,
  hasRecoverAsChoice,
  historyLabel,
  historyWhere,
  isPreTicked,
  otherToggleLabel,
  recoverAsOptions,
  type RecoverGroup,
} from "../utils/recoverModel";
import "./RecoverDialog.css";

interface Props {
  items: SessionHistoryItem[];
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  client: DaemonClient;
  onClose: () => void;
}

function initialTicks(items: SessionHistoryItem[]): Set<string> {
  return new Set(
    items.filter((i) => i.entry.mode !== "headless" && isPreTicked(i)).map(
      (i) => i.entry.session_id,
    ),
  );
}

/**
 * Lists ended sessions from the daemon's history and respawns the ticked ones
 * with their Claude conversation resumed. Sessions lost together (a tracer or
 * daemon crash) are grouped and pre-ticked.
 */
export default function RecoverDialog({
  items,
  repos,
  workspaces,
  client,
  onClose,
}: Props) {
  const [ticked, setTicked] = useState<Set<string>>(() => initialTicks(items));
  const [conversation, setConversation] = useState<Record<string, string>>({});
  const [recoverAs, setRecoverAs] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  // Set after a partial failure: only these rows stay listed, with their error.
  const [failures, setFailures] = useState<Map<string, string> | null>(null);
  const [requestError, setRequestError] = useState<string | null>(null);
  const [showOther, setShowOther] = useState(false);
  const dialogRef = useRef<HTMLDivElement | null>(null);

  useEscape(onClose);
  useAutoFocus(dialogRef);
  useFocusReturn();

  const groups = useMemo<RecoverGroup[]>(() => {
    const all = groupHistory(items);
    if (failures === null) return all;
    return all
      .map((g) => ({
        ...g,
        items: g.items.filter((i) => failures.has(i.entry.session_id)),
      }))
      .filter((g) => g.items.length > 0);
  }, [items, failures]);

  // After a partial failure every remaining row is shown, Other included.
  const otherShown = showOther || failures !== null;
  const isShown = useCallback(
    (group: RecoverGroup) => group.key !== "other" || otherShown,
    [otherShown],
  );

  const defaults = useMemo(
    () => defaultConversations(groups.flatMap((g) => g.items)),
    [groups],
  );

  const selectable = useMemo(
    () =>
      groups
        .filter(isShown)
        .flatMap((g) => g.items)
        .filter((i) => disabledReason(i) === null),
    [groups, isShown],
  );
  const selected = selectable.filter((i) => ticked.has(i.entry.session_id));

  const toggle = (id: string) => {
    setTicked((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const selectAll = () => {
    setTicked(new Set(selectable.map((i) => i.entry.session_id)));
  };

  const toRequest = useCallback(
    (item: SessionHistoryItem): RecoverItem => {
      const id = item.entry.session_id;
      const conversationId =
        conversation[id] ?? defaults.get(id) ?? item.candidates[0]?.id ?? null;
      if (!hasRecoverAsChoice(item)) {
        return { history_id: id, conversation_id: conversationId, how: { type: "claude" } };
      }
      const options = recoverAsOptions(item, repos);
      const chosen =
        options.find((o) => o.key === recoverAs[id]) ?? options[0];
      return {
        history_id: id,
        conversation_id: conversationId,
        how: chosen?.how ?? { type: "shell" },
      };
    },
    [conversation, defaults, recoverAs, repos],
  );

  const recover = useCallback(async () => {
    if (busy || selected.length === 0) return;
    setBusy(true);
    setRequestError(null);
    const reply = await recoverSessions(client, selected.map(toRequest));
    setBusy(false);
    if (!reply.ok) {
      setRequestError(`Recovery did not answer: ${reply.reason}`);
      return;
    }
    const failed = new Map<string, string>();
    for (const r of reply.results) {
      if (r.error !== null) failed.set(r.history_id, r.error);
    }
    if (failed.size === 0) {
      onClose();
      return;
    }
    setFailures(failed);
    setTicked(new Set(failed.keys()));
  }, [busy, selected, client, toRequest, onClose]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "Enter") return;
    const tag = (e.target as HTMLElement).tagName;
    // A select owns Enter to pick an option; a focused button's own
    // activation (Cancel, Select all) must win over "recover".
    if (tag === "SELECT" || tag === "BUTTON") return;
    e.preventDefault();
    void recover();
  };

  const now = new Date();

  return (
    <div className="modal-backdrop" data-testid="recover-dialog">
      <div
        ref={dialogRef}
        className="modal recover-dialog"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label="Recover sessions"
      >
        <header className="modal-header">
          <h2>Recover sessions</h2>
          <button
            type="button"
            className="link"
            onClick={onClose}
            aria-label="Cancel"
            data-testid="recover-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body recover-body">
          {groups.length === 0 ? (
            <p className="muted">No ended sessions to recover.</p>
          ) : (
            groups.map((group) => (
              <section key={group.key} className="recover-group">
                {group.key === "other" && failures === null ? (
                  <button
                    type="button"
                    className="link recover-group-title"
                    onClick={() => setShowOther((shown) => !shown)}
                    aria-expanded={otherShown}
                    data-testid="recover-other-toggle"
                  >
                    {otherToggleLabel(group.items.length, otherShown)}
                  </button>
                ) : (
                  <h3 className="recover-group-title">{group.title}</h3>
                )}
                {isShown(group) && (
                  <ul className="recover-list">
                    {group.items.map((item) => (
                      <RecoverRow
                        key={item.entry.session_id}
                        item={item}
                        repos={repos}
                        workspaces={workspaces}
                        now={now}
                        busy={busy}
                        ticked={ticked.has(item.entry.session_id)}
                        onToggle={toggle}
                        conversationId={
                          conversation[item.entry.session_id] ??
                          defaults.get(item.entry.session_id)
                        }
                        onConversation={(id, value) =>
                          setConversation((c) => ({ ...c, [id]: value }))
                        }
                        recoverAsKey={recoverAs[item.entry.session_id]}
                        onRecoverAs={(id, value) =>
                          setRecoverAs((c) => ({ ...c, [id]: value }))
                        }
                        failure={failures?.get(item.entry.session_id) ?? null}
                      />
                    ))}
                  </ul>
                )}
              </section>
            ))
          )}
          {requestError && (
            <p className="error-text" data-testid="recover-request-error">
              {requestError}
            </p>
          )}
        </div>
        <footer className="modal-footer">
          <button type="button" onClick={onClose} data-testid="recover-cancel">
            Cancel
          </button>
          <button
            type="button"
            onClick={selectAll}
            disabled={busy || selectable.length === 0}
            data-testid="recover-select-all"
          >
            Select all
          </button>
          <button
            type="button"
            className="primary"
            onClick={() => void recover()}
            disabled={busy || selected.length === 0}
            data-testid="recover-submit"
          >
            {busy ? "Recovering…" : `Recover ${selected.length}`}
          </button>
        </footer>
      </div>
    </div>
  );
}

interface RowProps {
  item: SessionHistoryItem;
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  now: Date;
  busy: boolean;
  ticked: boolean;
  onToggle: (id: string) => void;
  conversationId: string | undefined;
  onConversation: (id: string, value: string) => void;
  recoverAsKey: string | undefined;
  onRecoverAs: (id: string, value: string) => void;
  failure: string | null;
}

function RecoverRow({
  item,
  repos,
  workspaces,
  now,
  busy,
  ticked,
  onToggle,
  conversationId,
  onConversation,
  recoverAsKey,
  onRecoverAs,
  failure,
}: RowProps) {
  const id = item.entry.session_id;
  const reason = disabledReason(item);
  const disabled = reason !== null || busy;
  const options = hasRecoverAsChoice(item) ? recoverAsOptions(item, repos) : [];
  return (
    <li
      className={`recover-row${reason !== null ? " recover-row-disabled" : ""}`}
      data-history-id={id}
      data-testid="recover-row"
    >
      <label className="recover-row-main">
        <input
          type="checkbox"
          checked={reason === null && ticked}
          disabled={disabled}
          onChange={() => onToggle(id)}
        />
        <span className="recover-row-label">{historyLabel(item)}</span>
        <span className="muted small recover-row-meta">
          {historyWhere(item, repos, workspaces)} · {endedText(item, now)}
        </span>
      </label>
      {reason === null && (
        <div className="recover-row-controls">
          {item.candidates.length > 0 && (
            <select
              value={conversationId ?? item.candidates[0]?.id}
              disabled={busy}
              onChange={(e) => onConversation(id, e.target.value)}
              aria-label="Conversation"
              data-testid="recover-conversation"
            >
              {item.candidates.map((c) => (
                <option key={c.id} value={c.id}>
                  {candidateLabel(c, now)}
                </option>
              ))}
            </select>
          )}
          {options.length > 0 && (
            <label className="recover-row-as small">
              Recover as{" "}
              <select
                value={recoverAsKey ?? options[0]?.key}
                disabled={busy}
                onChange={(e) => onRecoverAs(id, e.target.value)}
                data-testid="recover-as"
              >
                {options.map((o) => (
                  <option key={o.key} value={o.key}>
                    {o.label}
                  </option>
                ))}
              </select>
            </label>
          )}
        </div>
      )}
      {reason !== null && (
        <div className="muted small recover-row-reason">{reason}</div>
      )}
      {failure !== null && (
        <div className="error-text small" data-testid="recover-row-error">
          {failure}
        </div>
      )}
    </li>
  );
}
