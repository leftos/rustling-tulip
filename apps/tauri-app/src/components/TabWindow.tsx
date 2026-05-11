import { useCallback, useState } from "react";
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

  return (
    <div className="tab-window">
      <header className="tab-window-header">
        <h2>{tab.name}</h2>
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
        />
      )}
    </div>
  );
}
