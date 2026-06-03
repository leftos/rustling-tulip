import { useState } from "react";
import type { RearrangeLayout } from "../types";
import { useEscape, useFocusReturn } from "../utils/a11y";
import {
  bestFitGridCols,
  gridArrangements,
  paneAreaAspect,
} from "../utils/grid";

interface Props {
  incomingCount: number;
  localCount: number;
  onImport: (layout: RearrangeLayout, maxPerTab: number | null) => void;
  onKeep: () => void;
  onEmpty: () => void;
}

type LayoutMode = "grid" | "horizontal" | "vertical";

/// Shown when the client reconnects to a remote daemon whose session count
/// differs from the count the client last saw. Lets the user decide how to
/// arrange the incoming sessions before any tabs are committed.
export default function ImportArrangementModal({
  incomingCount,
  localCount,
  onImport,
  onKeep,
  onEmpty,
}: Props) {
  useEscape(onKeep);
  useFocusReturn();

  const [arranging, setArranging] = useState(false);
  const [mode, setMode] = useState<LayoutMode>("grid");
  const [gridColsRaw, setGridColsRaw] = useState<number>(0);
  // 0 = unlimited
  const [maxPerTabRaw, setMaxPerTabRaw] = useState<string>("");

  const aspect = paneAreaAspect();
  const autoCols = bestFitGridCols(incomingCount, aspect);
  const autoRows = Math.ceil(incomingCount / autoCols);

  const shapeOptions: Array<{ cols: number; label: string }> = [
    { cols: 0, label: `Auto (${autoCols} × ${autoRows})` },
    ...gridArrangements(incomingCount).map((s) => ({
      cols: s.cols,
      label: `${s.cols} × ${Math.ceil(incomingCount / s.cols)}`,
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

  const onConfirmImport = () => {
    const layout: RearrangeLayout =
      mode === "horizontal"
        ? { kind: "horizontal" }
        : mode === "vertical"
          ? { kind: "vertical" }
          : { kind: "grid", cols: gridCols === 0 ? autoCols : gridCols };
    onImport(layout, maxPerTab);
  };

  const delta = incomingCount - localCount;
  const deltaLabel =
    delta > 0 ? `+${delta} more` : `${Math.abs(delta)} fewer`;

  return (
    <div className="modal-backdrop" data-testid="import-arrangement-modal">
      <div
        className="modal modal-narrow"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Import remote sessions"
      >
        <header className="modal-header">
          <h2>Import remote sessions</h2>
          <button
            type="button"
            className="modal-close"
            onClick={onKeep}
            aria-label="Keep existing layout"
            title="Keep existing layout"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p className="settings-section-hint">
            The remote has{" "}
            <strong>
              {incomingCount} session{incomingCount === 1 ? "" : "s"}
            </strong>{" "}
            ({deltaLabel} than before). How would you like to set up this
            window?
          </p>

          <ul className="connection-list" data-testid="import-arrangement-options">
            <li>
              <button
                type="button"
                className="connection-option"
                onClick={onKeep}
                data-testid="import-keep"
              >
                <span className="connection-option-name">
                  Keep existing layout
                </span>
                <span className="connection-option-detail">
                  your tabs stay as they are
                </span>
              </button>
            </li>

            <li className="connection-row connection-discovered">
              <button
                type="button"
                className={
                  arranging ? "connection-option is-active" : "connection-option"
                }
                onClick={() => setArranging((v) => !v)}
                aria-expanded={arranging}
                data-testid="import-open-all"
              >
                <span className="connection-option-name">
                  Open all {incomingCount} session
                  {incomingCount === 1 ? "" : "s"}
                </span>
                <span className="connection-option-detail">
                  arrange them now
                </span>
              </button>

              {arranging && (
                <div className="connection-pair">
                  <fieldset className="move-panes-layout">
                    <legend>Layout</legend>
                    <label className="radio">
                      <input
                        type="radio"
                        name="import-layout"
                        checked={mode === "grid"}
                        onChange={() => setMode("grid")}
                        data-testid="import-layout-grid"
                      />
                      <span>Grid</span>
                    </label>
                    {mode === "grid" && (
                      <select
                        className="move-panes-grid-shape"
                        value={gridCols}
                        onChange={(e) => setGridColsRaw(Number(e.target.value))}
                        aria-label="Grid shape"
                        data-testid="import-grid-shape"
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
                        name="import-layout"
                        checked={mode === "horizontal"}
                        onChange={() => setMode("horizontal")}
                        data-testid="import-layout-horizontal"
                      />
                      <span>Side by side</span>
                    </label>
                    <label className="radio">
                      <input
                        type="radio"
                        name="import-layout"
                        checked={mode === "vertical"}
                        onChange={() => setMode("vertical")}
                        data-testid="import-layout-vertical"
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
                      placeholder={`${incomingCount} (all in one tab)`}
                      value={maxPerTabRaw}
                      onChange={(e) => setMaxPerTabRaw(e.target.value)}
                      data-testid="import-max-per-tab"
                    />
                  </label>

                  <div className="connection-add-actions">
                    <button
                      type="button"
                      className="primary"
                      onClick={onConfirmImport}
                      data-testid="import-confirm"
                    >
                      Open {incomingCount} session
                      {incomingCount === 1 ? "" : "s"}
                      {maxPerTab !== null
                        ? `, ${Math.ceil(incomingCount / maxPerTab)} tab${Math.ceil(incomingCount / maxPerTab) === 1 ? "" : "s"}`
                        : ""}
                    </button>
                  </div>
                </div>
              )}
            </li>

            <li>
              <button
                type="button"
                className="connection-option"
                onClick={onEmpty}
                data-testid="import-empty"
              >
                <span className="connection-option-name">Start empty</span>
                <span className="connection-option-detail">
                  add sessions from the sidebar yourself
                </span>
              </button>
            </li>
          </ul>
        </div>
      </div>
    </div>
  );
}
