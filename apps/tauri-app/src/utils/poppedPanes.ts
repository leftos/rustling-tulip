const STORAGE_KEY = "rt.popped-panes";
const CHANGE_EVENT = "rt:popped-panes-changed";

type PoppedPaneMap = Record<string, true>;

function readMap(): PoppedPaneMap {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return {};
    }
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return {};
    }
    const out: PoppedPaneMap = {};
    for (const [key, value] of Object.entries(parsed)) {
      if (value === true) {
        out[key] = true;
      }
    }
    return out;
  } catch {
    return {};
  }
}

function writeMap(next: PoppedPaneMap): void {
  try {
    if (Object.keys(next).length === 0) {
      localStorage.removeItem(STORAGE_KEY);
    } else {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
    }
  } catch {
    return;
  }
  window.dispatchEvent(new CustomEvent(CHANGE_EVENT));
}

export function isPanePoppedOut(paneId: string): boolean {
  return readMap()[paneId] === true;
}

export function markPanePoppedOut(paneId: string): void {
  const next = readMap();
  if (next[paneId] === true) {
    return;
  }
  next[paneId] = true;
  writeMap(next);
}

export function clearPanePoppedOut(paneId: string): void {
  const next = readMap();
  if (next[paneId] !== true) {
    return;
  }
  delete next[paneId];
  writeMap(next);
}

export function prunePoppedOutPanes(keep: Set<string>): void {
  const current = readMap();
  let dirty = false;
  for (const paneId of Object.keys(current)) {
    if (keep.has(paneId)) {
      continue;
    }
    delete current[paneId];
    dirty = true;
  }
  if (dirty) {
    writeMap(current);
  }
}

export function subscribePoppedPaneChanges(cb: () => void): () => void {
  const onStorage = (event: StorageEvent) => {
    if (event.key !== STORAGE_KEY) {
      return;
    }
    cb();
  };
  window.addEventListener(CHANGE_EVENT, cb);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(CHANGE_EVENT, cb);
    window.removeEventListener("storage", onStorage);
  };
}
