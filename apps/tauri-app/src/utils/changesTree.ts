import type { GitFileChange } from "../types";

/**
 * Hierarchical projection of a flat `GitFileChange[]` list for the
 * source-control sidebar's `ChangesTree` view.
 *
 * Each `FolderNode` is a single tree row that may represent one *or more*
 * directory segments concatenated (`buildChangesTree` folds single-child
 * intermediate folders the way VSCode does — `apps/tauri-app/src/components`
 * becomes one row, not four). `path` is the joined display label; the
 * `fullPath` is the absolute repo-relative directory path that the renderer
 * uses for expand/collapse state keying.
 *
 * Children come in two forms: nested folders + leaf files. The whole tree
 * is recomputed from scratch on every `changes` change; with a few hundred
 * paths and a flat fan-out the cost is negligible compared to the diff
 * round-trip the user is actually waiting on.
 */
export interface FolderNode {
  /// Display label — may include slashes when intermediate folders were
  /// folded. Always the *deepest* folder's name plus any folded ancestors.
  label: string;
  /// Repo-relative path of the deepest folder this node represents. Used as
  /// the key for expand/collapse state.
  fullPath: string;
  folders: FolderNode[];
  files: GitFileChange[];
}

interface MutFolder {
  /// The undisputed final segment for this folder (no folding yet).
  name: string;
  fullPath: string;
  children: Map<string, MutFolder>;
  files: GitFileChange[];
}

/**
 * Build the hierarchical changes tree from a flat `GitFileChange[]`.
 *
 * Steps:
 *   1. Split each change's path into segments and bucket files under the
 *      deepest containing directory.
 *   2. Walk the mutable tree depth-first and collapse runs of single-child
 *      directories (no files, exactly one subdir child) into joined labels.
 *
 * Files at the root land in a synthetic root node whose `label` is `""`;
 * the renderer treats that node's children as top-level rows.
 */
export function buildChangesTree(changes: GitFileChange[]): FolderNode {
  const root: MutFolder = {
    name: "",
    fullPath: "",
    children: new Map(),
    files: [],
  };

  for (const c of changes) {
    const segments = c.path.split(/[/\\]/).filter((s) => s.length > 0);
    if (segments.length <= 1) {
      root.files.push(c);
      continue;
    }
    let cursor = root;
    for (let i = 0; i < segments.length - 1; i++) {
      const seg = segments[i]!;
      const fullPath =
        cursor.fullPath === "" ? seg : `${cursor.fullPath}/${seg}`;
      let next = cursor.children.get(seg);
      if (!next) {
        next = { name: seg, fullPath, children: new Map(), files: [] };
        cursor.children.set(seg, next);
      }
      cursor = next;
    }
    cursor.files.push(c);
  }

  return collapse(root);
}

function collapse(node: MutFolder): FolderNode {
  /// Fold runs of single-child directories: if a folder has no files and
  /// exactly one child folder, absorb that child's label into ours and
  /// recurse with the grandchild. Repeat until the invariant breaks.
  let current = node;
  while (
    current.files.length === 0 &&
    current.children.size === 1 &&
    // Don't fold the synthetic root — keeping its empty label means the
    // renderer can spread its children at the top level cleanly.
    current.fullPath !== ""
  ) {
    const onlyChild = current.children.values().next().value as MutFolder;
    current = {
      name: `${current.name}/${onlyChild.name}`,
      fullPath: onlyChild.fullPath,
      children: onlyChild.children,
      files: onlyChild.files,
    };
  }

  const folders: FolderNode[] = [];
  // Sort children by name so the tree is deterministic.
  const orderedNames = Array.from(current.children.keys()).sort();
  for (const name of orderedNames) {
    folders.push(collapse(current.children.get(name)!));
  }
  /// Files sorted alphabetically by basename for the same reason.
  const files = [...current.files].sort((a, b) =>
    a.path.localeCompare(b.path),
  );
  return {
    label: current.name,
    fullPath: current.fullPath,
    folders,
    files,
  };
}
