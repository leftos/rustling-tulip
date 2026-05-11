import { useMemo, useState } from "react";
import type { GitFileChange } from "../../types";
import { buildChangesTree, type FolderNode } from "../../utils/changesTree";

interface Props {
  changes: GitFileChange[];
  selectedPath: string | null;
  onSelect: (path: string) => void;
}

/**
 * Path-folded hierarchical view of working-tree changes.
 *
 * Renders folders + files via recursive `<TreeRow>`s. Folders are
 * click-to-collapse (no chevron-only target — the whole row toggles).
 * Files emit a click → parent's `onSelect(path)` so the diff pane updates.
 *
 * Expand state is tracked locally as a `Set<string>` of *collapsed* paths,
 * matching the existing Sessions sidebar's idiom — folders default to
 * expanded. Not persisted across reloads on purpose; the user's selection
 * is the more important piece of state here.
 */
export default function ChangesTree({
  changes,
  selectedPath,
  onSelect,
}: Props) {
  const tree = useMemo(() => buildChangesTree(changes), [changes]);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const toggle = (path: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  return (
    <ul
      className="changes-tree"
      data-testid="source-control-changes-tree"
    >
      {tree.folders.map((f) => (
        <FolderRow
          key={f.fullPath}
          node={f}
          depth={0}
          collapsed={collapsed}
          onToggle={toggle}
          selectedPath={selectedPath}
          onSelect={onSelect}
        />
      ))}
      {tree.files.map((c) => (
        <FileRow
          key={c.path}
          change={c}
          depth={0}
          selected={c.path === selectedPath}
          onSelect={onSelect}
        />
      ))}
    </ul>
  );
}

interface FolderRowProps {
  node: FolderNode;
  depth: number;
  collapsed: Set<string>;
  onToggle: (path: string) => void;
  selectedPath: string | null;
  onSelect: (path: string) => void;
}

function FolderRow({
  node,
  depth,
  collapsed,
  onToggle,
  selectedPath,
  onSelect,
}: FolderRowProps) {
  const isCollapsed = collapsed.has(node.fullPath);
  const fileCount = countFiles(node);
  return (
    <li>
      <div
        className="tree-row changes-tree-folder"
        style={{ paddingLeft: 4 + depth * 12 }}
        onClick={() => onToggle(node.fullPath)}
        role="button"
        tabIndex={0}
        aria-expanded={!isCollapsed}
        aria-label={`Folder ${node.fullPath} (${fileCount} changes)`}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onToggle(node.fullPath);
          }
        }}
        data-testid="source-control-changes-folder"
        data-folder-path={node.fullPath}
      >
        <span className="tree-caret" aria-hidden="true">
          {isCollapsed ? "▸" : "▾"}
        </span>
        <span className="tree-label" title={node.fullPath}>
          {node.label}
        </span>
        <span className="list-item-meta small">{fileCount}</span>
      </div>
      {!isCollapsed && (
        <ul className="tree-children">
          {node.folders.map((sub) => (
            <FolderRow
              key={sub.fullPath}
              node={sub}
              depth={depth + 1}
              collapsed={collapsed}
              onToggle={onToggle}
              selectedPath={selectedPath}
              onSelect={onSelect}
            />
          ))}
          {node.files.map((c) => (
            <FileRow
              key={c.path}
              change={c}
              depth={depth + 1}
              selected={c.path === selectedPath}
              onSelect={onSelect}
            />
          ))}
        </ul>
      )}
    </li>
  );
}

interface FileRowProps {
  change: GitFileChange;
  depth: number;
  selected: boolean;
  onSelect: (path: string) => void;
}

function FileRow({ change, depth, selected, onSelect }: FileRowProps) {
  const basename = change.path.split(/[/\\]/).pop() ?? change.path;
  const classes = [
    "tree-row",
    "tree-leaf",
    "changes-tree-file",
    selected ? "selected" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <li>
      <div
        className={classes}
        style={{ paddingLeft: 4 + depth * 12 }}
        onClick={() => onSelect(change.path)}
        role="button"
        tabIndex={0}
        aria-label={`${statusToWords(change.status)} ${change.path}`}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onSelect(change.path);
          }
        }}
        data-testid="source-control-changes-row"
        data-file-path={change.path}
      >
        <span className={`file-status status-${change.status}`}>
          {change.status}
        </span>
        <span className="tree-label" title={change.path}>
          {basename}
        </span>
      </div>
    </li>
  );
}

function countFiles(node: FolderNode): number {
  let total = node.files.length;
  for (const f of node.folders) total += countFiles(f);
  return total;
}

function statusToWords(status: string): string {
  switch (status) {
    case "M":
      return "Modified";
    case "A":
      return "Added";
    case "D":
      return "Deleted";
    case "R":
      return "Renamed";
    case "U":
      return "Untracked";
    default:
      return status;
  }
}
