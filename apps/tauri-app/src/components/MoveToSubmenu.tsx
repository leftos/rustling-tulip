import type { DaemonClient } from "../api";
import { tabGrid, type TabEntry } from "../types";
import { pickBalancedDropTarget } from "../utils/grid";
import { MenuSubmenu } from "./MenuSubmenu";

interface Props {
  tabs: TabEntry[];
  /// Tab the pane currently lives in (the move source).
  sourceTabId: string;
  /// Pane being moved.
  sourcePaneId: string;
  client: DaemonClient;
  /// Arms the App-level focus-switch so the freshly created tab activates
  /// after the daemon broadcasts it (see App.tsx `onArmNextNewTab`).
  onArmNextNewTab: () => void;
  onClose: () => void;
  /// Disambiguates the data-testids across hosts (e.g. "session-context"
  /// for the session menu, "pane-context" for the empty-pane menu).
  testIdPrefix: string;
  /// Tabs to omit from the "existing tabs" list. Defaults to just the source
  /// tab; the session menu passes every tab the session is bound to.
  excludeTabIds?: Set<string>;
  onTabsSnapshotUndo?: (
    tabs: TabEntry[],
    message: string,
    restoreFocusedPaneId: string | null,
  ) => void;
}

/// Shared "Move to ▸ {New tab, existing tabs…}" submenu used by both the
/// session context menu (pane header + sidebar) and the empty-pane context
/// menu. "New tab" extracts the pane into a fresh tab; existing-tab targets
/// move the pane in via the auto-balanced drop placement.
export function MoveToSubmenu({
  tabs,
  sourceTabId,
  sourcePaneId,
  client,
  onArmNextNewTab,
  onClose,
  testIdPrefix,
  excludeTabIds,
  onTabsSnapshotUndo,
}: Props) {
  const exclude = excludeTabIds ?? new Set([sourceTabId]);
  const moveTargets = tabs.filter((t) => !exclude.has(t.id));

  // New tab deliberately registers no undo entry: the created tab's id isn't
  // known at dispatch time, so the snapshot-restore undo path can't remove it
  // (see App.tsx `onUndo`). Mirrors the header extract button.
  const onMoveToNewTab = () => {
    onArmNextNewTab();
    client.send({
      type: "extract_to_new_tab",
      source_tab_id: sourceTabId,
      pane_ids: [sourcePaneId],
      name: null,
    });
    onClose();
  };

  const onMoveToExisting = (dstTab: TabEntry) => {
    const dstGrid = tabGrid(dstTab);
    if (!dstGrid) return;
    // Smart-balanced destination: an empty pane absorbs the move in place;
    // otherwise the largest pane splits along its longer axis.
    const dst = pickBalancedDropTarget(dstGrid);
    if (!dst) return;
    const sourceTab = tabs.find((t) => t.id === sourceTabId) ?? null;
    if (onTabsSnapshotUndo && sourceTab) {
      onTabsSnapshotUndo([sourceTab, dstTab], "Moved pane", sourcePaneId);
    }
    client.send({
      type: "move_pane",
      src_tab_id: sourceTabId,
      src_pane_id: sourcePaneId,
      dst_tab_id: dstTab.id,
      dst_pane_id: dst.paneId,
      edge: dst.edge,
    });
    onClose();
  };

  return (
    <MenuSubmenu label="Move to" dataTestId={`${testIdPrefix}-move-menu`}>
      <li>
        <button
          type="button"
          onClick={onMoveToNewTab}
          title="Move this pane into a fresh tab"
          data-testid={`${testIdPrefix}-move-to-new-tab`}
        >
          New tab
        </button>
      </li>
      {moveTargets.length > 0 && (
        <li className="context-menu-separator" aria-hidden="true" />
      )}
      {moveTargets.map((t) => (
        <li key={`mv:${t.id}`}>
          <button
            type="button"
            onClick={() => onMoveToExisting(t)}
            title={`Move this pane into "${t.name}"`}
            data-testid={`${testIdPrefix}-move-to-tab`}
            data-tab-id={t.id}
          >
            {t.name}
          </button>
        </li>
      ))}
    </MenuSubmenu>
  );
}
