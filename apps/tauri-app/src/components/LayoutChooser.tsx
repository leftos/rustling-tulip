import { useState } from "react";
import type { ClonableLayout, InitLayoutKind, RearrangeLayout } from "../types";
import {
  bestFitGridCols,
  gridArrangements,
  paneAreaAspect,
} from "../utils/grid";

export interface ArrangementConfig {
  layout: RearrangeLayout;
  maxPerTab: number | null;
}

interface Props {
  hasLegacy: boolean;
  activeSessionCount: number;
  clonable: ClonableLayout[];
  /// When `kind` is `all_sessions`, `arrangement` carries the user-chosen
  /// layout + max-per-tab so the caller can apply it once the daemon replies
  /// with `tabs`. For all other kinds, `arrangement` is undefined.
  onChoose: (kind: InitLayoutKind, arrangement?: ArrangementConfig) => void;
}

type LayoutMode = "grid" | "horizontal" | "vertical";

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
  // When the user clicks "Open all sessions" we expand an in-place arrangement
  // picker before committing so they can choose grid shape and max per tab.
  const [pickingArrangement, setPickingArrangement] = useState(false);
  const [mode, setMode] = useState<LayoutMode>("grid");
  const [gridColsRaw, setGridColsRaw] = useState<number>(0);
  const [maxPerTabRaw, setMaxPerTabRaw] = useState<string>("");

  const aspect = paneAreaAspect();
  const autoCols = bestFitGridCols(activeSessionCount, aspect);
  const autoRows = Math.ceil(activeSessionCount / autoCols);

  const shapeOptions: Array<{ cols: number; label: string }> = [
    { cols: 0, label: `Auto (${autoCols} × ${autoRows})` },
    ...gridArrangements(activeSessionCount).map((s) => ({
      cols: s.cols,
      label: `${s.cols} × ${Math.ceil(activeSessionCount / s.cols)}`,
    })),
  ];

  const gridCols = shapeOptions.some((o) => o.cols === gridColsRaw)
    ? gridColsRaw
    : 0;

  const parsedMaxPerTab = parseInt(maxPerTabRaw, 10);
  const maxPerTab =
    maxPerTabRaw.trim() === "" || isNaN(parsedMaxPerTab) || parsedMaxPerTab < 1
      ? null
      : parsedMaxPerTab;

  const onConfirmAllSessions = () => {
    const layout: RearrangeLayout =
      mode === "horizontal"
        ? { kind: "horizontal" }
        : mode === "vertical"
          ? { kind: "vertical" }
          : { kind: "grid", cols: gridCols === 0 ? autoCols : gridCols };
    onChoose({ kind: "all_sessions" }, { layout, maxPerTab });
  };

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
              <li className="connection-row connection-discovered">
                <button
                  type="button"
                  className={
                    pickingArrangement
                      ? "connection-option is-active"
                      : "connection-option"
                  }
                  onClick={() => setPickingArrangement((v) => !v)}
                  aria-expanded={pickingArrangement}
                  data-testid="layout-choose-all"
                >
                  <span className="connection-option-name">
                    Open all active sessions
                  </span>
                  <span className="connection-option-detail">
                    one pane per running session ({activeSessionCount})
                  </span>
                </button>

                {pickingArrangement && (
                  <div className="connection-pair">
                    <fieldset className="move-panes-layout">
                      <legend>Layout</legend>
                      <label className="radio">
                        <input
                          type="radio"
                          name="chooser-layout"
                          checked={mode === "grid"}
                          onChange={() => setMode("grid")}
                          data-testid="chooser-layout-grid"
                        />
                        <span>Grid</span>
                      </label>
                      {mode === "grid" && (
                        <select
                          className="move-panes-grid-shape"
                          value={gridCols}
                          onChange={(e) =>
                            setGridColsRaw(Number(e.target.value))
                          }
                          aria-label="Grid shape"
                          data-testid="chooser-grid-shape"
                        >
                          {shapeOptions.map((o) => (
                            <option key={o.cols} value={o.cols}>
                              {o.label}
                            </option>
                          ))}
                        </select>
                      )}
                      <label className="radio">
                        <input
                          type="radio"
                          name="chooser-layout"
                          checked={mode === "horizontal"}
                          onChange={() => setMode("horizontal")}
                          data-testid="chooser-layout-horizontal"
                        />
                        <span>Side by side</span>
                      </label>
                      <label className="radio">
                        <input
                          type="radio"
                          name="chooser-layout"
                          checked={mode === "vertical"}
                          onChange={() => setMode("vertical")}
                          data-testid="chooser-layout-vertical"
                        />
                        <span>Stacked</span>
                      </label>
                    </fieldset>

                    <label className="move-panes-name">
                      <span>Max sessions per tab</span>
                      <input
                        type="number"
                        min={1}
                        max={64}
                        placeholder={`${activeSessionCount} (all in one tab)`}
                        value={maxPerTabRaw}
                        onChange={(e) => setMaxPerTabRaw(e.target.value)}
                        data-testid="chooser-max-per-tab"
                      />
                    </label>

                    <div className="connection-add-actions">
                      <button
                        type="button"
                        className="primary"
                        onClick={onConfirmAllSessions}
                        data-testid="chooser-confirm-all"
                      >
                        Open {activeSessionCount} session
                        {activeSessionCount === 1 ? "" : "s"}
                        {maxPerTab !== null
                          ? `, ${Math.ceil(activeSessionCount / maxPerTab)} tab${Math.ceil(activeSessionCount / maxPerTab) === 1 ? "" : "s"}`
                          : ""}
                      </button>
                    </div>
                  </div>
                )}
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
