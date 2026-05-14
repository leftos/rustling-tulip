import { useEffect, useState } from "react";
import type { AppearanceOverrides, CodexSandbox, PermissionMode } from "../types";

const DISABLE_NOTIFICATIONS = import.meta.env["VITE_RT_E2E"] === "1";

/// User-facing preferences persisted via localStorage. Stored as a single
/// JSON-serialised object under `STORAGE_KEY` so loading is atomic. The
/// shape carries an explicit `version` so we can migrate cleanly later
/// (today: bump it, write a `migrate_v1_v2` step, point `loadSettings` at
/// the upgrader before returning). Notifications and spawn defaults gate
/// behaviour at render-time; sidebar defaultView is read on Sidebar mount.

export interface Settings {
  version: 1;
  notifications: {
    /// Fire an OS notification when a session enters `awaiting_input`.
    awaiting_input: boolean;
    /// Fire when a session transitions to `stopped` from a non-terminal
    /// state.
    stopped: boolean;
    /// Fire when a session enters the `error` state.
    error: boolean;
  };
  sidebar: {
    /// Which tree organisation the sidebar starts in on a fresh window.
    /// Was previously a standalone localStorage key (`rt.sidebar.view`);
    /// `loadSettings` migrates that key on first load.
    default_view: "container" | "tab";
  };
  spawn: {
    /// Initial value of the SpawnDialog trusted-launch checkbox. Fresh
    /// installs default to `false`; saved settings keep their stored value
    /// through `mergeWithDefaults`.
    skip_permissions_default: boolean;
    /// Pre-fill for the spawn dialog's permission-mode dropdown (claude
    /// only; ignored when `skip_permissions_default` is on). `null` means
    /// "no flag" — falls through to claude's own default.
    default_permission_mode: PermissionMode | null;
    /// Pre-fill for the spawn dialog's codex-sandbox dropdown (codex only;
    /// ignored when trusted launch is on).
    default_codex_sandbox: CodexSandbox | null;
    /// Folder used by the sidebar's quick standalone-shell action. `null`
    /// lets the daemon choose its platform default.
    standalone_shell_default_dir: string | null;
  };
  appearance: AppearanceOverrides;
  /// Last custom colours picked through any appearance colour input.
  /// Preset clicks do not enter this list; it exists for manual recall.
  appearance_recent_colors: string[];
  terminal: {
    /// Auto-copy any non-empty terminal selection to the system
    /// clipboard. Opt-out — defaults to `true` because most users
    /// expect PuTTY / GNOME-terminal style selection-copy. Bare Ctrl+C
    /// still copies the current selection (and falls through to ^C
    /// when nothing is selected) regardless of this setting.
    copy_on_selection: boolean;
  };
}

export const DEFAULT_SETTINGS: Settings = {
  version: 1,
  notifications: {
    awaiting_input: !DISABLE_NOTIFICATIONS,
    stopped: !DISABLE_NOTIFICATIONS,
    error: !DISABLE_NOTIFICATIONS,
  },
  sidebar: {
    default_view: "container",
  },
  spawn: {
    skip_permissions_default: false,
    default_permission_mode: null,
    default_codex_sandbox: null,
    standalone_shell_default_dir: null,
  },
  appearance: {
    accent_color: null,
    terminal_background_color: null,
    terminal_frame_color: null,
    terminal_font_family: null,
    terminal_font_size: 13,
    terminal_font_bold: false,
  },
  appearance_recent_colors: [],
  terminal: {
    copy_on_selection: true,
  },
};

const STORAGE_KEY = "rt.settings";
/// Legacy standalone key for the sidebar view toggle. Read once during
/// migration and then ignored. We don't delete it: older builds still see
/// the value, and the storage cost is trivial.
const LEGACY_SIDEBAR_VIEW_KEY = "rt.sidebar.view";
/// DOM event fired on every settings write so cross-component listeners
/// (notification gate, Sidebar default-view, SpawnDialog initial state)
/// can re-read without prop-drilling. Components that care subscribe in
/// `useSettings`.
const CHANGE_EVENT = "rt:settings-changed";

export function loadSettings(): Settings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw === null) {
      // First boot for this user (or post-clear): seed from defaults,
      // honoring the legacy sidebar-view key so users who set the old toggle
      // don't lose their preference on upgrade.
      const seeded = { ...DEFAULT_SETTINGS };
      const legacyView = localStorage.getItem(LEGACY_SIDEBAR_VIEW_KEY);
      if (legacyView === "container" || legacyView === "tab") {
        seeded.sidebar = { default_view: legacyView };
      }
      saveSettings(seeded);
      return seeded;
    }
    const parsed = JSON.parse(raw) as Partial<Settings> & {
      terminal?: Partial<Settings["terminal"]> & {
        font_size?: unknown;
        font_family?: unknown;
        font_bold?: unknown;
      };
    };
    return mergeWithDefaults(parsed);
  } catch {
    // Corrupted JSON or storage unavailable — fall back to defaults
    // without overwriting whatever's there, so a manual repair stays
    // possible.
    return { ...DEFAULT_SETTINGS };
  }
}

export function saveSettings(next: Settings): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
    window.dispatchEvent(
      new CustomEvent<Settings>(CHANGE_EVENT, { detail: next }),
    );
  } catch {
    // Storage write failed (private-mode quirk, quota exceeded). We
    // can't recover here; the in-memory state still reflects the user's
    // change for this session.
  }
}

/// Returns a tuple of `[settings, setSettings]` where `setSettings`
/// persists *and* broadcasts. Subscribes to `CHANGE_EVENT` so multiple
/// components stay in sync when one writes (e.g. modal toggles a
/// notification setting while the attention handler reads it).
export function useSettings(): [Settings, (next: Settings) => void] {
  const [settings, setLocal] = useState<Settings>(() => loadSettings());
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<Settings>).detail;
      setLocal(mergeWithDefaults(detail));
    };
    window.addEventListener(CHANGE_EVENT, handler);
    return () => window.removeEventListener(CHANGE_EVENT, handler);
  }, []);
  return [settings, saveSettings];
}

export function rememberRecentCustomColor(color: string): void {
  const normalized = normalizeSettingsColor(color);
  if (normalized === null) return;
  const settings = loadSettings();
  const existing = settings.appearance_recent_colors.filter(
    (candidate) => candidate !== normalized,
  );
  saveSettings({
    ...settings,
    appearance_recent_colors: [normalized, ...existing].slice(0, 12),
  });
}

/// Merge a partial (possibly older-version) object with the current
/// defaults. Keeps known fields the caller supplied, fills missing
/// ones from the default. Doesn't attempt deep version migration yet
/// — when `version` advances, add a switch here.
function mergeWithDefaults(
  partial: Partial<Settings> & {
    terminal?: Partial<Settings["terminal"]> & {
      font_size?: unknown;
      font_family?: unknown;
      font_bold?: unknown;
    };
  },
): Settings {
  const def = DEFAULT_SETTINGS;
  const appearance = {
    ...def.appearance,
    ...partial.appearance,
  };
  if (
    partial.appearance?.terminal_font_size === undefined &&
    typeof partial.terminal?.font_size === "number"
  ) {
    appearance.terminal_font_size = partial.terminal.font_size;
  }
  if (
    partial.appearance?.terminal_font_family === undefined &&
    (typeof partial.terminal?.font_family === "string" ||
      partial.terminal?.font_family === null)
  ) {
    appearance.terminal_font_family = partial.terminal.font_family;
  }
  if (
    partial.appearance?.terminal_font_bold === undefined &&
    typeof partial.terminal?.font_bold === "boolean"
  ) {
    appearance.terminal_font_bold = partial.terminal.font_bold;
  }
  return {
    version: 1,
    notifications: {
      ...(DISABLE_NOTIFICATIONS
        ? def.notifications
        : { ...def.notifications, ...partial.notifications }),
    },
    sidebar: {
      ...def.sidebar,
      ...partial.sidebar,
    },
    spawn: {
      ...def.spawn,
      ...partial.spawn,
    },
    appearance,
    appearance_recent_colors: normalizeRecentColors(
      partial.appearance_recent_colors,
    ),
    terminal: {
      ...def.terminal,
      ...partial.terminal,
      copy_on_selection:
        partial.terminal?.copy_on_selection ?? def.terminal.copy_on_selection,
    },
  };
}

function normalizeRecentColors(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const colors: string[] = [];
  for (const candidate of value) {
    if (typeof candidate !== "string") continue;
    const normalized = normalizeSettingsColor(candidate);
    if (normalized === null || colors.includes(normalized)) continue;
    colors.push(normalized);
    if (colors.length >= 12) break;
  }
  return colors;
}

function normalizeSettingsColor(color: string): string | null {
  const trimmed = color.trim();
  if (!/^#[0-9a-fA-F]{6}$/.test(trimmed)) return null;
  return trimmed.toLowerCase();
}
