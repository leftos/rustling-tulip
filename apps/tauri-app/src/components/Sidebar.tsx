import type {
  ConnectionState,
} from "../api";
import type { RepoEntry, SessionSnapshot, WorkspaceEntry } from "../types";

interface Props {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  sessions: SessionSnapshot[];
  selectedSessionId: string | null;
  connection: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  onAddRepo: () => void;
  onRemoveRepo: (id: string) => void;
  onRemoveWorkspace: (id: string) => void;
  onSelectSession: (id: string) => void;
  onOpenSpawn: () => void;
  onOpenWorkspaceCreator: () => void;
}

export default function Sidebar(props: Props) {
  return (
    <aside className="sidebar">
      <header className="sidebar-header">
        <span className="brand">rustling-tulip</span>
      </header>

      <Section title="Sessions" actionLabel="+ New" onAction={props.onOpenSpawn}>
        {props.sessions.length === 0 ? (
          <Empty>none yet</Empty>
        ) : (
          <ul className="list">
            {props.sessions.map((s) => (
              <li
                key={s.id}
                className={
                  s.id === props.selectedSessionId
                    ? "list-item selected"
                    : "list-item"
                }
                onClick={() => props.onSelectSession(s.id)}
              >
                <span className={`status-dot status-${s.status}`} />
                <span className="list-item-label">{s.label}</span>
                <span className="list-item-meta">{s.kind === "workspace" ? "ws" : "1"}</span>
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Repos" actionLabel="+ Add" onAction={props.onAddRepo}>
        {props.repos.length === 0 ? (
          <Empty>none registered</Empty>
        ) : (
          <ul className="list">
            {props.repos.map((r) => (
              <li key={r.id} className="list-item static">
                <span className="list-item-label" title={r.path}>
                  {r.name}
                </span>
                <button
                  type="button"
                  className="list-item-action"
                  title="Remove repo"
                  onClick={() => props.onRemoveRepo(r.id)}
                >
                  ×
                </button>
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section
        title="Workspaces"
        actionLabel="+ New"
        onAction={
          props.repos.length >= 2 ? props.onOpenWorkspaceCreator : undefined
        }
      >
        {props.workspaces.length === 0 ? (
          <Empty>
            {props.repos.length < 2
              ? "register at least 2 repos first"
              : "none defined"}
          </Empty>
        ) : (
          <ul className="list">
            {props.workspaces.map((w) => (
              <li key={w.id} className="list-item static">
                <span className="list-item-label">{w.name}</span>
                <span className="list-item-meta">
                  {w.member_repo_ids.length}
                </span>
                <button
                  type="button"
                  className="list-item-action"
                  title="Remove workspace"
                  onClick={() => props.onRemoveWorkspace(w.id)}
                >
                  ×
                </button>
              </li>
            ))}
          </ul>
        )}
      </Section>
    </aside>
  );
}

type SectionProps = {
  title: string;
  children: React.ReactNode;
  actionLabel?: string | undefined;
  onAction?: (() => void) | undefined;
};

function Section(props: SectionProps) {
  return (
    <section className="sidebar-section">
      <div className="sidebar-section-header">
        <h2>{props.title}</h2>
        {props.actionLabel && props.onAction && (
          <button type="button" className="link" onClick={props.onAction}>
            {props.actionLabel}
          </button>
        )}
      </div>
      {props.children}
    </section>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return <p className="empty">{children}</p>;
}
