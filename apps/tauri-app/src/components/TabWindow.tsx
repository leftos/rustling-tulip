import { useCallback, useEffect, useState } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { DaemonClient } from "../api";
import type { SessionSnapshot, TabEntry } from "../types";
import GridRenderer from "./GridRenderer";
import DiffPane from "./DiffPane";

interface Props {
  tab: TabEntry;
  client: DaemonClient;
  sessions: SessionSnapshot[];
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
  hasRepos: boolean;
}

export default function TabWindow({
  tab,
  client,
  sessions,
  subscribePty,
  hasRepos,
}: Props) {
  const [focusedPaneId, setFocusedPaneId] = useState<string | null>(null);

  const onSpawnInPane = useCallback((_paneId: string) => {
    // Pop-out window has no SpawnDialog of its own; defer to the main
    // window by leaving this as a no-op. Sidebar-driven spawn still works
    // there because the daemon broadcasts tab_updated to both windows.
  }, []);

  const onCloseWindow = useCallback(() => {
    void getCurrentWebviewWindow().close();
  }, []);

  // Replace the OS window title (originally a raw UUID set by
  // `open_tab_window`) with the human-readable tab name. Updates on
  // rename so the taskbar entry stays in sync.
  useEffect(() => {
    void getCurrentWebviewWindow().setTitle(tab.name);
  }, [tab.name]);

  return (
    <div className="tab-window">
      <header className="tab-window-header">
        <h2 title={tab.name}>{tab.name}</h2>
        <span className="spacer" />
        <button
          type="button"
          onClick={onCloseWindow}
          title="Close this window. The tab and its sessions stay in the main window."
          data-testid="tab-window-close"
        >
          Close window
        </button>
      </header>
      {tab.content.kind === "diff" ? (
        <DiffPane
          client={client}
          repoId={tab.content.repo_id}
          path={tab.content.path}
          against={tab.content.against}
        />
      ) : (
        <GridRenderer
          tab={tab}
          client={client}
          sessions={sessions}
          subscribePty={subscribePty}
          focusedPaneId={focusedPaneId}
          onFocusPane={setFocusedPaneId}
          onSpawnInPane={onSpawnInPane}
          hasRepos={hasRepos}
          /* Pop-out window has no tab bar / no concept of activating tabs,
             so extract-to-new-tab from a pane here is best-effort: the new
             tab still gets created in the daemon's tab list and shows up
             in the main window; we just can't activate it from here. */
          onArmNextNewTab={() => {}}
        />
      )}
    </div>
  );
}
