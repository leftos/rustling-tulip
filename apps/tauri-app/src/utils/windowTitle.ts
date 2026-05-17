import type { SessionSnapshot, TabEntry } from "../types";
import { tabGrid } from "../types";
import { collectPanes } from "./grid";

const PRODUCT_NAME = "rustling-tulip";

/// Per-user toggles that shape the title string. Mirrors `Settings.title`
/// in `utils/settings.ts` — kept narrow so `computeWindowTitle` can be
/// imported by tests without dragging in localStorage.
export interface WindowTitleOptions {
  show_busy_count: boolean;
  show_product_suffix: boolean;
}

const DEFAULT_OPTIONS: WindowTitleOptions = {
  show_busy_count: true,
  show_product_suffix: true,
};

/// Compute the OS-window title for the main window. The format is
/// `(M/N) <ActiveTab> — rustling-tulip` where `M` counts non-idle
/// terminals in the active tab and `N` counts the total terminals in
/// that tab. Counts are intentionally scoped to the active tab so a
/// busy background tab doesn't permanently rouse the title bar; the
/// title reflects what the user is currently looking at.
///
/// `(M/N)` is dropped when the tab has no bound terminals (avoids a
/// noisy `(0/0)` on empty tabs). The product suffix can be toggled off
/// for users who want a tighter title.
export function computeWindowTitle(
  activeTab: TabEntry | null,
  sessions: SessionSnapshot[],
  options: WindowTitleOptions = DEFAULT_OPTIONS,
): string {
  const suffix = options.show_product_suffix ? ` — ${PRODUCT_NAME}` : "";
  if (!activeTab) {
    return options.show_product_suffix ? PRODUCT_NAME : "";
  }

  const grid = tabGrid(activeTab);
  let total = 0;
  let busy = 0;
  if (grid) {
    const sessionById = new Map(sessions.map((s) => [s.id, s] as const));
    for (const pane of collectPanes(grid)) {
      if (pane.session_id === null) continue;
      const session = sessionById.get(pane.session_id);
      if (!session || session.is_inactive) continue;
      total += 1;
      if (isBusy(session)) busy += 1;
    }
  }

  const prefix = options.show_busy_count && total > 0 ? `(${busy}/${total}) ` : "";
  return `${prefix}${activeTab.name}${suffix}`;
}

/// A session counts as "busy" when it's actively doing something the user
/// might want to know about: running a command, awaiting permission, or
/// still booting. Idle and exited sessions don't count — the dot is calm,
/// and the title should be too.
function isBusy(session: SessionSnapshot): boolean {
  return (
    session.status === "working" ||
    session.status === "awaiting_input" ||
    session.status === "spawning"
  );
}
