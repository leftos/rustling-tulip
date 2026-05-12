import { useEffect, useState } from "react";
import { loadSettings } from "./settings";

/// Persistent font-size overrides for terminal panes. The app-wide default
/// lives in `Settings.terminal.font_size`; per-tab and per-session overrides
/// live here in two small string→number maps because tab/session ids are
/// dynamic and don't belong inside the settings JSON.
///
/// Effective size for a Terminal mount is computed in
/// `resolveFontSize(sessionId, tabId)` — the resolver walks
/// session-override → tab-override → app-default in that order.
///
/// Writes broadcast a CustomEvent so any mounted Terminal can re-read
/// without prop-drilling. Each Terminal subscribes via `useFontSize`.

const TAB_OVERRIDES_KEY = "rt.terminal.font_size.tabs";
const SESSION_OVERRIDES_KEY = "rt.terminal.font_size.sessions";
const CHANGE_EVENT = "rt:font-size-changed";

const MIN_SIZE = 8;
const MAX_SIZE = 32;

interface OverrideMaps {
  byTab: Record<string, number>;
  bySession: Record<string, number>;
}

function clamp(size: number): number {
  if (!Number.isFinite(size)) return 13;
  return Math.max(MIN_SIZE, Math.min(MAX_SIZE, Math.round(size)));
}

function loadOverrides(): OverrideMaps {
  return {
    byTab: readMap(TAB_OVERRIDES_KEY),
    bySession: readMap(SESSION_OVERRIDES_KEY),
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

export function getAppDefaultFontSize(): number {
  return clamp(loadSettings().terminal.font_size);
}

export function getTabFontSize(tabId: string): number | null {
  const map = readMap(TAB_OVERRIDES_KEY);
  return map[tabId] ?? null;
}

export function getSessionFontSize(sessionId: string): number | null {
  const map = readMap(SESSION_OVERRIDES_KEY);
  return map[sessionId] ?? null;
}

/// Resolve the effective font size for a Terminal mount. Walks
/// session-override → tab-override (when `tabId` is non-null) →
/// app-default. `tabId` may be `null` in pop-out windows where the
/// per-tab override isn't accessible from the parent context — the
/// pop-out still respects the session and app-wide values.
export function resolveFontSize(
  sessionId: string | null,
  tabId: string | null,
): number {
  const overrides = loadOverrides();
  if (sessionId !== null) {
    const s = overrides.bySession[sessionId];
    if (typeof s === "number") return s;
  }
  if (tabId !== null) {
    const t = overrides.byTab[tabId];
    if (typeof t === "number") return t;
  }
  return getAppDefaultFontSize();
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

export function setSessionFontSize(sessionId: string, size: number): void {
  const map = readMap(SESSION_OVERRIDES_KEY);
  map[sessionId] = clamp(size);
  writeMap(SESSION_OVERRIDES_KEY, map);
  broadcast();
}

export function clearSessionFontSize(sessionId: string): void {
  const map = readMap(SESSION_OVERRIDES_KEY);
  if (!(sessionId in map)) return;
  delete map[sessionId];
  writeMap(SESSION_OVERRIDES_KEY, map);
  broadcast();
}

/// Drop overrides whose id is no longer in `liveTabIds` / `liveSessionIds`.
/// Called periodically from App.tsx so the localStorage map doesn't grow
/// unboundedly across long sessions.
export function pruneOverrides(
  liveTabIds: Set<string>,
  liveSessionIds: Set<string>,
): void {
  const overrides = loadOverrides();
  let changed = false;
  const tabOut: Record<string, number> = {};
  for (const [k, v] of Object.entries(overrides.byTab)) {
    if (liveTabIds.has(k)) tabOut[k] = v;
    else changed = true;
  }
  const sessionOut: Record<string, number> = {};
  for (const [k, v] of Object.entries(overrides.bySession)) {
    if (liveSessionIds.has(k)) sessionOut[k] = v;
    else changed = true;
  }
  if (!changed) return;
  writeMap(TAB_OVERRIDES_KEY, tabOut);
  writeMap(SESSION_OVERRIDES_KEY, sessionOut);
  broadcast();
}

/// Subscribe to font-size changes for a specific Terminal mount. Returns
/// the resolved effective size; re-runs the resolver on every change
/// event (app-default, tab-override, session-override, or settings).
export function useFontSize(
  sessionId: string | null,
  tabId: string | null,
): number {
  const [size, setSize] = useState(() => resolveFontSize(sessionId, tabId));
  useEffect(() => {
    const recompute = () => setSize(resolveFontSize(sessionId, tabId));
    recompute();
    window.addEventListener(CHANGE_EVENT, recompute);
    window.addEventListener("rt:settings-changed", recompute);
    return () => {
      window.removeEventListener(CHANGE_EVENT, recompute);
      window.removeEventListener("rt:settings-changed", recompute);
    };
  }, [sessionId, tabId]);
  return size;
}

export { MIN_SIZE as MIN_FONT_SIZE, MAX_SIZE as MAX_FONT_SIZE };
