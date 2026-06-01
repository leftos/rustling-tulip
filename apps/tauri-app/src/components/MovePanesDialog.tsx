import { useMemo, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import type { RearrangeLayout, SessionSnapshot, TabEntry } from "../types";
import { tabGrid } from "../types";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";
import {
  bestFitGridCols,
  collectPanes,
  gridArrangements,
  paneAreaAspect,
} from "../utils/grid";
import { sessionDisplayLabel } from "../utils/sessionLabel";

interface Props {
  /// Tab whose panes are being moved (the extraction source).
  tab: TabEntry;
  /// All known sessions, used to label each pane by its session title.
  sessions: SessionSnapshot[];
  client: DaemonClient;
  /// Arms the App-level focus-switch so the freshly created tab activates
  /// once the daemon broadcasts it (see App.tsx `onArmNextNewTab`).
  onArmNextNewTab: () => void;
  onClose: () => void;
}

type LayoutMode = "horizontal" | "vertical" | "grid";

/// Modal for moving a chosen subset of a tab's panes into a brand-new tab
/// arranged with a chosen layout. Only session-bound panes are listed (empty
/// placeholders aren't worth moving and the daemon rearrange drops them
/// anyway). The grid shape options are recomputed from how many panes are
/// currently selected, mirroring the tab "Rearrange panes" submenu.
export default function MovePanesDialog({
  tab,
  sessions,
  client,
  onArmNextNewTab,
  onClose,
}: Props) {
  const firstCheckboxRef = useRef<HTMLInputElement | null>(null);
  useEscape(onClose);
  useAutoFocus(firstCheckboxRef);
  useFocusReturn();

  const sessionsById = useMemo(() => {
    const map = new Map<string, SessionSnapshot>();
    for (const s of sessions) map.set(s.id, s);
    return map;
  }, [sessions]);

  // Bound panes in grid (left-to-right) order; this order is preserved into
  // the new tab so the moved panes keep their relative arrangement.
  const panes = useMemo(() => {
    const grid = tabGrid(tab);
    if (!grid) return [];
    return collectPanes(grid).filter((p) => p.session_id !== null);
  }, [tab]);

  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [mode, setMode] = useState<LayoutMode>("horizontal");
  // 0 == auto (aspect-aware best fit); otherwise an explicit column count.
  const [gridColsRaw, setGridColsRaw] = useState<number>(0);
  const [name, setName] = useState<string>("");

  const selectedCount = selected.size;

  // Aspect ratio of the pane area the new tab will occupy, measured once on
  // open; drives the aspect-aware "Auto" column count.
  const aspect = useMemo(() => paneAreaAspect(), []);
  const autoCols = bestFitGridCols(selectedCount, aspect);

  const shapeOptions = useMemo(() => {
    const auto = autoCols;
    const autoRows = Math.ceil(selectedCount / auto);
    const opts: Array<{ cols: number; label: string }> = [
      {
        cols: 0,
        label:
          selectedCount > 0 ? `Auto (${auto} cols × ${autoRows} rows)` : "Auto",
      },
    ];
    for (const s of gridArrangements(selectedCount)) {
      opts.push({ cols: s.cols, label: `${s.cols} cols × ${s.rows} rows` });
    }
    return opts;
  }, [selectedCount, autoCols]);

  // The selected column count may no longer be valid after the selection
  // shrinks; fall back to Auto when it drops out of the option list.
  const gridCols = shapeOptions.some((o) => o.cols === gridColsRaw)
    ? gridColsRaw
    : 0;

  const toggle = (paneId: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(paneId)) next.delete(paneId);
      else next.add(paneId);
      return next;
    });
  };

  const paneLabel = (sessionId: string | null): string => {
    if (!sessionId) return "Empty pane";
    const s = sessionsById.get(sessionId);
    return s ? sessionDisplayLabel(s) : sessionId;
  };

  const onConfirm = () => {
    if (selectedCount === 0) return;
    const paneIds = panes
      .filter((p) => selected.has(p.pane_id))
      .map((p) => p.pane_id);
    // gridCols 0 is the "Auto" sentinel; resolve it to the aspect-aware
    // best-fit column count so the new tab honors the window shape.
    const layout: RearrangeLayout =
      mode === "horizontal"
        ? { kind: "horizontal" }
        : mode === "vertical"
          ? { kind: "vertical" }
          : { kind: "grid", cols: gridCols === 0 ? autoCols : gridCols };
    onArmNextNewTab();
    client.send({
      type: "extract_to_new_tab",
      source_tab_id: tab.id,
      pane_ids: paneIds,
      name: name.trim() || null,
      layout,
    });
    onClose();
  };

  return (
    <div className="modal-backdrop" data-testid="move-panes-dialog">
      <div
        className="modal modal-narrow"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Move panes to a new tab"
      >
        <header className="modal-header">
          <h2>Move panes to a new tab</h2>
          <button
            type="button"
            className="link"
            onClick={onClose}
            aria-label="Cancel"
            data-testid="move-panes-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p className="settings-section-hint">
            Pick the panes to move out of "{tab.name}". They're removed from
            this tab and arranged in a new one with the layout below.
          </p>
          <ul className="move-panes-list" data-testid="move-panes-list">
            {panes.map((p, i) => (
              <li key={p.pane_id}>
                <label className="checkbox">
                  <input
                    ref={i === 0 ? firstCheckboxRef : undefined}
                    type="checkbox"
                    checked={selected.has(p.pane_id)}
                    onChange={() => toggle(p.pane_id)}
                    data-testid="move-panes-pane"
                    data-pane-id={p.pane_id}
                  />
                  <span className="move-panes-pane-label">
                    {paneLabel(p.session_id)}
                  </span>
                </label>
              </li>
            ))}
          </ul>

          <fieldset className="move-panes-layout">
            <legend>Layout</legend>
            <label className="radio">
              <input
                type="radio"
                name="move-panes-layout"
                checked={mode === "horizontal"}
                onChange={() => setMode("horizontal")}
                data-testid="move-panes-layout-horizontal"
              />
              <span>Side by side</span>
            </label>
            <label className="radio">
              <input
                type="radio"
                name="move-panes-layout"
                checked={mode === "vertical"}
                onChange={() => setMode("vertical")}
                data-testid="move-panes-layout-vertical"
              />
              <span>Stacked</span>
            </label>
            <label className="radio">
              <input
                type="radio"
                name="move-panes-layout"
                checked={mode === "grid"}
                onChange={() => setMode("grid")}
                data-testid="move-panes-layout-grid"
              />
              <span>Grid</span>
            </label>
            {mode === "grid" && (
              <select
                className="move-panes-grid-shape"
                value={gridCols}
                onChange={(e) => setGridColsRaw(Number(e.target.value))}
                aria-label="Grid shape"
                data-testid="move-panes-grid-shape"
              >
                {shapeOptions.map((o) => (
                  <option key={o.cols} value={o.cols}>
                    {o.label}
                  </option>
                ))}
              </select>
            )}
          </fieldset>

          <label className="move-panes-name">
            <span>Name (optional)</span>
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="New tab name"
              data-testid="move-panes-name"
            />
          </label>
        </div>
        <footer className="modal-footer">
          <button type="button" onClick={onClose} data-testid="move-panes-cancel">
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            onClick={onConfirm}
            disabled={selectedCount === 0}
            data-testid="move-panes-confirm"
          >
            {selectedCount === 0
              ? "Move panes"
              : `Move ${selectedCount} pane${selectedCount === 1 ? "" : "s"}`}
          </button>
        </footer>
      </div>
    </div>
  );
}
