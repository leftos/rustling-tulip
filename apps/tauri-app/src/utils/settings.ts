import { useEffect, useState } from "react";
import type { CodexSandbox, PermissionMode } from "../types";

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
    /// Was a standalone localStorage key (`rt.sidebar.view`) pre-iter-49;
    /// `loadSettings` migrates that key on first load.
    default_view: "container" | "tab";
  };
  spawn: {
    /// Initial value of the SpawnDialog "skip permissions" / "yolo"
    /// checkbox. Defaults to `true` because the pre-iter-49 code did the
    /// same hard-coded.
    skip_permissions_default: boolean;
    /// Pre-fill for the spawn dialog's permission-mode dropdown (claude
    /// only; ignored when `skip_permissions_default` is on). `null` means
    /// "no flag" — falls through to claude's own default.
    default_permission_mode: PermissionMode | null;
    /// Pre-fill for the spawn dialog's codex-sandbox dropdown (codex
    /// only; ignored when `skip_permissions_default` is on, which becomes
    /// `--yolo` for codex).
    default_codex_sandbox: CodexSandbox | null;
  };
  terminal: {
    /// App-wide default font size in pixels. Per-tab and per-session
    /// overrides live in separate localStorage keys (see
    /// `utils/fontSize.ts`) because they're keyed by ephemeral ids.
    /// Clamped to [8, 32] by the UI.
    font_size: number;
    /// App-wide terminal font family. `null` keeps the historical
    /// `'Geist Mono', 'Cascadia Mono', Consolas, …` cascade defined in
    /// Terminal.tsx. Bundled families (Fira Code, JetBrains Mono,
    /// Cascadia Code) and system-picked families both stash a plain
    /// family-name string here; xterm resolves it against
    /// `document.fonts`.
    font_family: string | null;
    /// Render normal-weight text at the bold weight. xterm's
    /// `fontWeight` option — independent from selection / ANSI bold,
    /// which always use `fontWeightBold`.
    font_bold: boolean;
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
    awaiting_input: true,
    stopped: true,
    error: true,
  },
  sidebar: {
    default_view: "container",
  },
  spawn: {
    skip_permissions_default: true,
    default_permission_mode: null,
    default_codex_sandbox: null,
  },
  terminal: {
    font_size: 13,
    font_family: null,
    font_bold: false,
    copy_on_selection: true,
  },
};

const STORAGE_KEY = "rt.settings";
/// Pre-iter-49 standalone key for the sidebar view toggle. Read once
/// during migration and then ignored. We don't delete it: a downgrade to
/// a pre-iter-49 build still sees the value, and the storage cost is
/// trivial.
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
      // honoring the legacy sidebar-view key so users who set the toggle
      // pre-iter-49 don't lose their preference on upgrade.
      const seeded = { ...DEFAULT_SETTINGS };
      const legacyView = localStorage.getItem(LEGACY_SIDEBAR_VIEW_KEY);
      if (legacyView === "container" || legacyView === "tab") {
        seeded.sidebar = { default_view: legacyView };
      }
      saveSettings(seeded);
      return seeded;
    }
    const parsed = JSON.parse(raw) as Partial<Settings>;
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
      setLocal(detail);
    };
    window.addEventListener(CHANGE_EVENT, handler);
    return () => window.removeEventListener(CHANGE_EVENT, handler);
  }, []);
  return [settings, saveSettings];
}

/// Merge a partial (possibly older-version) object with the current
/// defaults. Keeps known fields the caller supplied, fills missing
/// ones from the default. Doesn't attempt deep version migration yet
/// — when `version` advances, add a switch here.
function mergeWithDefaults(partial: Partial<Settings>): Settings {
  const def = DEFAULT_SETTINGS;
  return {
    version: 1,
    notifications: {
      ...def.notifications,
      ...partial.notifications,
    },
    sidebar: {
      ...def.sidebar,
      ...partial.sidebar,
    },
    spawn: {
      ...def.spawn,
      ...partial.spawn,
    },
    terminal: {
      ...def.terminal,
      ...partial.terminal,
    },
  };
}
