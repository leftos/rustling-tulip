import type { ClonableLayout, InitLayoutKind } from "../types";

interface Props {
  hasLegacy: boolean;
  activeSessionCount: number;
  clonable: ClonableLayout[];
  onChoose: (kind: InitLayoutKind) => void;
}

/// First-connect chooser: a brand-new client (new install / new machine) picks
/// how to seed its own tab/pane layout over the daemon's shared sessions. The
/// daemon sends `layout_init_required`; the reply (`init_layout`) creates the
/// layout and the daemon answers with `tabs`. Mandatory — no dismiss; the user
/// must pick one (Start empty is always available).
export default function LayoutChooser({
  hasLegacy,
  activeSessionCount,
  clonable,
  onChoose,
}: Props) {
  return (
    <div className="modal-backdrop" data-testid="layout-chooser">
      <div
        className="modal modal-narrow"
        role="dialog"
        aria-modal="true"
        aria-label="Choose a layout"
      >
        <header className="modal-header">
          <h2>Set up this window</h2>
        </header>
        <div className="modal-body">
          <p className="settings-section-hint">
            This is a new client. Sessions are shared with every connected
            window, but each curates its own tabs. How should this window start?
          </p>
          <ul className="connection-list" data-testid="layout-chooser-options">
            <li>
              <button
                type="button"
                className="connection-option"
                onClick={() => onChoose({ kind: "empty" })}
                data-testid="layout-choose-empty"
              >
                <span className="connection-option-name">Start empty</span>
                <span className="connection-option-detail">
                  add the sessions you want yourself
                </span>
              </button>
            </li>
            {activeSessionCount > 0 && (
              <li>
                <button
                  type="button"
                  className="connection-option"
                  onClick={() => onChoose({ kind: "all_sessions" })}
                  data-testid="layout-choose-all"
                >
                  <span className="connection-option-name">
                    Open all active sessions
                  </span>
                  <span className="connection-option-detail">
                    one pane per running session ({activeSessionCount})
                  </span>
                </button>
              </li>
            )}
            {hasLegacy && (
              <li>
                <button
                  type="button"
                  className="connection-option"
                  onClick={() => onChoose({ kind: "clone_legacy" })}
                  data-testid="layout-choose-legacy"
                >
                  <span className="connection-option-name">
                    Adopt the previous layout
                  </span>
                  <span className="connection-option-detail">
                    the tabs from before per-window layouts
                  </span>
                </button>
              </li>
            )}
            {clonable.map((c) => (
              <li key={c.client_id}>
                <button
                  type="button"
                  className="connection-option"
                  onClick={() =>
                    onChoose({ kind: "clone_client", client_id: c.client_id })
                  }
                  data-testid={`layout-choose-clone-${c.client_id}`}
                >
                  <span className="connection-option-name">
                    Copy {c.name ?? "another window"}'s layout
                  </span>
                  <span className="connection-option-detail">
                    same sessions, your own independent panes
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      </div>
    </div>
  );
}
