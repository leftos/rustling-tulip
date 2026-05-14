import { useEffect, useState } from "react";

/// Persistent font-size overrides for terminal panes. App / repo /
/// workspace / session defaults live in appearance settings; tab overrides
/// stay here because tabs are view layout, not session identity.
///
/// Effective size for a Terminal mount is computed in
/// `resolveFontSize(tabId, inheritedSize)` — the resolver walks
/// tab-override → inherited appearance size.
///
/// Writes broadcast a CustomEvent so any mounted Terminal can re-read
/// without prop-drilling. Each Terminal subscribes via `useFontSize`.

const TAB_OVERRIDES_KEY = "rt.terminal.font_size.tabs";
const CHANGE_EVENT = "rt:font-size-changed";

const MIN_SIZE = 8;
const MAX_SIZE = 32;

interface OverrideMaps {
  byTab: Record<string, number>;
}

function clamp(size: number): number {
  if (!Number.isFinite(size)) return 13;
  return Math.max(MIN_SIZE, Math.min(MAX_SIZE, Math.round(size)));
}

function loadOverrides(): OverrideMaps {
  return {
    byTab: readMap(TAB_OVERRIDES_KEY),
  };
}

function readMap(key: string): Record<string, number> {
  try {
    const raw = localStorage.getItem(key);
    if (raw === null) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (parsed === null || typeof parsed !== "object") return {};
    const out: Record<string, number> = {};
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof v === "number" && Number.isFinite(v)) {
        out[k] = clamp(v);
      }
    }
    return out;
  } catch {
    return {};
  }
}

function writeMap(key: string, map: Record<string, number>): void {
  try {
    localStorage.setItem(key, JSON.stringify(map));
  } catch {
    /// Quota / private-mode failure — the in-memory state still reflects
    /// the change for this session, just won't survive a reload.
  }
}

function broadcast(): void {
  window.dispatchEvent(new CustomEvent(CHANGE_EVENT));
}

export function getTabFontSize(tabId: string): number | null {
  const map = readMap(TAB_OVERRIDES_KEY);
  return map[tabId] ?? null;
}

/// Resolve the effective font size for a Terminal mount. Walks
/// tab-override (when `tabId` is non-null) → inherited appearance default.
/// `tabId` may be `null` in pop-out windows where the per-tab override
/// isn't accessible from the parent context.
export function resolveFontSize(tabId: string | null, inheritedSize: number): number {
  const overrides = loadOverrides();
  if (tabId !== null) {
    const t = overrides.byTab[tabId];
    if (typeof t === "number") return t;
  }
  return clamp(inheritedSize);
}

export function setTabFontSize(tabId: string, size: number): void {
  const map = readMap(TAB_OVERRIDES_KEY);
  map[tabId] = clamp(size);
  writeMap(TAB_OVERRIDES_KEY, map);
  broadcast();
}

export function clearTabFontSize(tabId: string): void {
  const map = readMap(TAB_OVERRIDES_KEY);
  if (!(tabId in map)) return;
  delete map[tabId];
  writeMap(TAB_OVERRIDES_KEY, map);
  broadcast();
}

/// Drop overrides whose id is no longer in `liveTabIds` / `liveSessionIds`.
/// Called periodically from App.tsx so the localStorage map doesn't grow
/// unboundedly across long sessions.
export function pruneOverrides(
  liveTabIds: Set<string>,
  _liveSessionIds: Set<string>,
): void {
  const overrides = loadOverrides();
  let changed = false;
  const tabOut: Record<string, number> = {};
  for (const [k, v] of Object.entries(overrides.byTab)) {
    if (liveTabIds.has(k)) tabOut[k] = v;
    else changed = true;
  }
  if (!changed) return;
  writeMap(TAB_OVERRIDES_KEY, tabOut);
  broadcast();
}

/// Subscribe to font-size changes for a specific Terminal mount. Returns
/// the resolved effective size; re-runs the resolver on every change
/// event (app-default, tab-override, session-override, or settings).
export function useFontSize(tabId: string | null, inheritedSize: number): number {
  const [size, setSize] = useState(() => resolveFontSize(tabId, inheritedSize));
  useEffect(() => {
    const recompute = () => setSize(resolveFontSize(tabId, inheritedSize));
    recompute();
    window.addEventListener(CHANGE_EVENT, recompute);
    window.addEventListener("rt:settings-changed", recompute);
    return () => {
      window.removeEventListener(CHANGE_EVENT, recompute);
      window.removeEventListener("rt:settings-changed", recompute);
    };
  }, [tabId, inheritedSize]);
  return size;
}

export { MIN_SIZE as MIN_FONT_SIZE, MAX_SIZE as MAX_FONT_SIZE };
