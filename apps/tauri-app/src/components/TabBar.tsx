import { useCallback } from "react";
import type { DaemonClient } from "../api";
import type { TabEntry } from "../types";

interface Props {
  tabs: TabEntry[];
  activeTabId: string | null;
  client: DaemonClient;
  onActivate: (tabId: string) => void;
}

export default function TabBar({ tabs, activeTabId, client, onActivate }: Props) {
  const onNewTab = useCallback(() => {
    client.send({ type: "create_tab", name: null, initial_session_id: null });
  }, [client]);

  const onCloseTab = useCallback(
    (tabId: string, e: React.MouseEvent) => {
      e.stopPropagation();
      client.send({ type: "close_tab", tab_id: tabId });
    },
    [client],
  );

  if (tabs.length === 0) {
    return (
      <div className="tab-bar tab-bar-empty">
        <button type="button" className="tab-bar-new" onClick={onNewTab}>
          + New tab
        </button>
      </div>
    );
  }

  return (
    <div className="tab-bar">
      <div className="tab-bar-list" role="tablist">
        {tabs.map((t) => {
          const isActive = t.id === activeTabId;
          const classes = ["tab-pill", isActive ? "is-active" : ""]
            .filter(Boolean)
            .join(" ");
          return (
            <div
              key={t.id}
              className={classes}
              role="tab"
              aria-selected={isActive}
              onClick={() => onActivate(t.id)}
            >
              <span className="tab-pill-label" title={t.name}>
                {t.name}
              </span>
              <button
                type="button"
                className="tab-pill-close"
                title="Close tab"
                onClick={(e) => onCloseTab(t.id, e)}
              >
                ×
              </button>
            </div>
          );
        })}
      </div>
      <button
        type="button"
        className="tab-bar-new"
        title="New tab"
        onClick={onNewTab}
      >
        +
      </button>
    </div>
  );
}
