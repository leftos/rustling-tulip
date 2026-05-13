import { useCallback, useEffect, useRef, useState } from "react";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import type { CodexSandbox, PermissionMode } from "../types";
import { logToFile } from "../utils/logger";
import { useEscape, useFocusReturn } from "../utils/a11y";
import { saveSettings, type Settings } from "../utils/settings";
import { MAX_FONT_SIZE, MIN_FONT_SIZE } from "../utils/fontSize";
import { BUNDLED_FONTS, isBundledFont } from "../utils/bundledFonts";

interface Props {
  settings: Settings;
  onClose: () => void;
}

type PermissionState = "granted" | "denied" | "default" | "unknown";

/// Settings modal — a thin localStorage-backed configuration surface.
/// State persistence runs through `saveSettings` in `utils/settings.ts`,
/// which writes localStorage AND fires a `rt:settings-changed` window event
/// so cross-component subscribers re-read on every write without prop-drilling.
export default function SettingsModal({ settings, onClose }: Props) {
  const closeRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onClose);
  useFocusReturn();

  // Edit-in-place: we mutate a local copy and call `saveSettings` on each
  // change so the user sees instant feedback without a separate Save /
  // Apply button.
  const update = useCallback(
    (mut: (s: Settings) => Settings) => {
      saveSettings(mut(settings));
    },
    [settings],
  );

  const [permission, setPermission] = useState<PermissionState>("unknown");
  const [permissionPending, setPermissionPending] = useState(false);
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const granted = await isPermissionGranted();
      if (cancelled) return;
      setPermission(granted ? "granted" : "default");
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const onRequestPermission = useCallback(() => {
    setPermissionPending(true);
    void (async () => {
      try {
        const result = await requestPermission();
        setPermission(result as PermissionState);
        logToFile("info", `settings: requestPermission -> ${result}`);
      } catch (err) {
        logToFile("error", `settings: requestPermission threw: ${String(err)}`);
      } finally {
        setPermissionPending(false);
      }
    })();
  }, []);

  // Focus the Close button by default — Settings is a non-destructive
  // surface and we don't have a single "primary" action to autofocus.
  // Close is the safest landing point for keyboard users.
  useEffect(() => {
    closeRef.current?.focus();
  }, []);

  return (
    <div
      className="modal-backdrop"
      data-testid="settings-modal"
    >
      <div
        className="modal modal-wide"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Settings"
      >
        <header className="modal-header">
          <h2>Settings</h2>
          <button
            type="button"
            className="modal-close"
            onClick={onClose}
            aria-label="Close dialog"
            title="Close"
          >
            ×
          </button>
        </header>
        <div className="modal-body settings-body">
          <section
            className="settings-section"
            data-testid="settings-section-notifications"
          >
            <h3>Notifications</h3>
            <div className="settings-row">
              <span>Permission</span>
              <div className="settings-row-control">
                <PermissionBadge state={permission} />
                <button
                  type="button"
                  onClick={onRequestPermission}
                  disabled={permissionPending || permission === "granted"}
                  data-testid="settings-permission-request"
                  title={
                    permission === "granted"
                      ? "Already granted — managed by the OS now. Toggle via System Settings if you need to revoke."
                      : "Re-request OS notification permission"
                  }
                >
                  {permissionPending
                    ? "Requesting…"
                    : permission === "granted"
                      ? "Granted"
                      : "Request permission"}
                </button>
              </div>
            </div>
            <p className="settings-section-hint">
              Fire an OS notification when a session transitions to:
            </p>
            <Toggle
              testid="settings-notify-awaiting-input"
              label="Awaiting input"
              checked={settings.notifications.awaiting_input}
              onChange={(v) =>
                update((s) => ({
                  ...s,
                  notifications: { ...s.notifications, awaiting_input: v },
                }))
              }
            />
            <Toggle
              testid="settings-notify-stopped"
              label="Stopped"
              checked={settings.notifications.stopped}
              onChange={(v) =>
                update((s) => ({
                  ...s,
                  notifications: { ...s.notifications, stopped: v },
                }))
              }
            />
            <Toggle
              testid="settings-notify-error"
              label="Errored"
              checked={settings.notifications.error}
              onChange={(v) =>
                update((s) => ({
                  ...s,
                  notifications: { ...s.notifications, error: v },
                }))
              }
            />
          </section>

          <section
            className="settings-section"
            data-testid="settings-section-sidebar"
          >
            <h3>Sidebar</h3>
            <div className="settings-row">
              <span>Default view</span>
              <div
                className="sidebar-view-toggle"
                role="radiogroup"
                aria-label="Default sidebar view"
              >
                <button
                  type="button"
                  role="radio"
                  aria-checked={
                    settings.sidebar.default_view === "container"
                  }
                  className={
                    settings.sidebar.default_view === "container"
                      ? "active"
                      : ""
                  }
                  onClick={() =>
                    update((s) => ({
                      ...s,
                      sidebar: { default_view: "container" },
                    }))
                  }
                  data-testid="settings-sidebar-view-container"
                >
                  Repos
                </button>
                <button
                  type="button"
                  role="radio"
                  aria-checked={settings.sidebar.default_view === "tab"}
                  className={
                    settings.sidebar.default_view === "tab" ? "active" : ""
                  }
                  onClick={() =>
                    update((s) => ({
                      ...s,
                      sidebar: { default_view: "tab" },
                    }))
                  }
                  data-testid="settings-sidebar-view-tab"
                >
                  Tabs
                </button>
              </div>
            </div>
            <p className="settings-section-hint">
              Applied on a fresh window. The in-window toggle in the sidebar
              header still works per-session.
            </p>
          </section>

          <section
            className="settings-section"
            data-testid="settings-section-spawn"
          >
            <h3>Spawn defaults</h3>
            <p className="settings-section-hint">
              Pre-fill these in the spawn dialog. You can override on every
              spawn.
            </p>
            <Toggle
              testid="settings-spawn-skip-permissions"
              label="Trusted launch by default"
              checked={settings.spawn.skip_permissions_default}
              onChange={(v) =>
                update((s) => ({
                  ...s,
                  spawn: { ...s.spawn, skip_permissions_default: v },
                }))
              }
            />
            <p className="settings-section-hint">
              When enabled, new Claude and Codex sessions bypass approval
              prompts. Codex also bypasses sandboxing.
            </p>
            <div className="settings-row">
              <span>Claude approval mode</span>
              <select
                value={settings.spawn.default_permission_mode ?? ""}
                onChange={(e) => {
                  const value = e.target.value;
                  const next: PermissionMode | null =
                    value === ""
                      ? null
                      : (value as PermissionMode);
                  update((s) => ({
                    ...s,
                    spawn: { ...s.spawn, default_permission_mode: next },
                  }));
                }}
                disabled={settings.spawn.skip_permissions_default}
                data-testid="settings-spawn-permission-mode"
                title={
                  settings.spawn.skip_permissions_default
                    ? "Ignored while trusted launch is on"
                    : ""
                }
              >
                <option value="">(none — claude's default)</option>
                <option value="default">default</option>
                <option value="accept_edits">accept edits</option>
                <option value="bypass_permissions">bypass permissions</option>
                <option value="plan">plan</option>
              </select>
            </div>
            <div className="settings-row">
              <span>Codex sandbox mode</span>
              <select
                value={settings.spawn.default_codex_sandbox ?? ""}
                onChange={(e) => {
                  const value = e.target.value;
                  const next: CodexSandbox | null =
                    value === ""
                      ? null
                      : (value as CodexSandbox);
                  update((s) => ({
                    ...s,
                    spawn: { ...s.spawn, default_codex_sandbox: next },
                  }));
                }}
                disabled={settings.spawn.skip_permissions_default}
                data-testid="settings-spawn-codex-sandbox"
                title={
                  settings.spawn.skip_permissions_default
                    ? "Ignored while trusted launch is on"
                    : ""
                }
              >
                <option value="">(none — codex's default)</option>
                <option value="read-only">read-only</option>
                <option value="workspace-write">workspace-write</option>
                <option value="danger-full-access">danger-full-access</option>
              </select>
            </div>
          </section>

          <section
            className="settings-section"
            data-testid="settings-section-terminal"
          >
            <h3>Terminal</h3>
            <TerminalFontFamilyControl
              value={settings.terminal.font_family}
              onChange={(family) =>
                update((s) => ({
                  ...s,
                  terminal: { ...s.terminal, font_family: family },
                }))
              }
            />
            <Toggle
              testid="settings-terminal-font-bold"
              label="Render terminal text in bold"
              checked={settings.terminal.font_bold}
              onChange={(v) =>
                update((s) => ({
                  ...s,
                  terminal: { ...s.terminal, font_bold: v },
                }))
              }
            />
            <div className="settings-row">
              <span>Font size</span>
              <div className="settings-row-control">
                <input
                  type="range"
                  min={MIN_FONT_SIZE}
                  max={MAX_FONT_SIZE}
                  step={1}
                  value={settings.terminal.font_size}
                  onChange={(e) => {
                    const v = Number.parseInt(e.target.value, 10);
                    if (!Number.isFinite(v)) return;
                    update((s) => ({
                      ...s,
                      terminal: { ...s.terminal, font_size: v },
                    }));
                  }}
                  data-testid="settings-terminal-font-size"
                  aria-label="Terminal font size"
                />
                <span className="muted small">
                  {settings.terminal.font_size}px
                </span>
              </div>
            </div>
            <p className="settings-section-hint">
              App-wide default. Per-tab and per-session overrides are set
              from the tab and session right-click menus, or with
              Ctrl+= / Ctrl+- (current tab) and Ctrl+Shift+= /
              Ctrl+Shift+- (focused session).
            </p>
            <Toggle
              testid="settings-terminal-copy-on-selection"
              label="Copy selection to clipboard automatically"
              checked={settings.terminal.copy_on_selection}
              onChange={(v) =>
                update((s) => ({
                  ...s,
                  terminal: { ...s.terminal, copy_on_selection: v },
                }))
              }
            />
            <p className="settings-section-hint">
              Off: select to highlight, copy explicitly with Ctrl+C
              (when there's a selection) or Ctrl+Shift+C.
            </p>
          </section>
        </div>
        <footer className="modal-footer">
          <button
            ref={closeRef}
            type="button"
            className="primary"
            onClick={onClose}
            data-testid="settings-close"
          >
            Close
          </button>
        </footer>
      </div>
    </div>
  );
}

/// Minimal shape of the FontData entry returned by `queryLocalFonts()`.
/// The TypeScript DOM lib doesn't ship the Local Font Access API yet so
/// we declare just the field we read. Cast through `unknown` at the
/// call site to avoid leaking the global type.
interface LocalFontData {
  family: string;
}

/// Window augmentation for the still-experimental Local Font Access API.
/// Available in Chromium-based webviews (Edge WebView2, Tauri's default
/// on Windows; Chrome on macOS via WebKit's polyfill stays absent — we
/// gate the UI on this feature-detect).
interface WindowWithLocalFonts {
  queryLocalFonts?: () => Promise<LocalFontData[]>;
}

function TerminalFontFamilyControl({
  value,
  onChange,
}: {
  value: string | null;
  onChange: (next: string | null) => void;
}) {
  // `null` selection = "use the default cascade in Terminal.tsx". We
  // surface it as the first option so users can roll back any picked
  // font without remembering its name.
  const [systemFonts, setSystemFonts] = useState<string[] | null>(null);
  const [systemFontsError, setSystemFontsError] = useState<string | null>(
    null,
  );
  const canQueryLocalFonts =
    typeof window !== "undefined" &&
    typeof (window as WindowWithLocalFonts).queryLocalFonts === "function";

  const loadSystemFonts = useCallback(async () => {
    const w = window as WindowWithLocalFonts;
    if (!w.queryLocalFonts) {
      setSystemFontsError(
        "Local Font Access API isn't available in this webview.",
      );
      return;
    }
    try {
      const fonts = await w.queryLocalFonts();
      // Dedupe by family — the API returns one entry per face (weight /
      // style), but the user picks at the family granularity.
      const families = Array.from(new Set(fonts.map((f) => f.family))).sort(
        (a, b) => a.localeCompare(b),
      );
      setSystemFonts(families);
      setSystemFontsError(null);
    } catch (err) {
      setSystemFontsError(String(err));
    }
  }, []);

  const handleSelectChange = (next: string) => {
    onChange(next === "" ? null : next);
  };

  // A current font that's neither bundled nor in the freshly fetched
  // system list still has to render in the <select>; otherwise the
  // displayed value silently snaps back to the default. We tag it as a
  // "custom" option in its own optgroup.
  const valueIsKnown =
    value === null ||
    isBundledFont(value) ||
    (systemFonts?.includes(value) ?? false);

  return (
    <>
      <div className="settings-row">
        <span>Font family</span>
        <div className="settings-row-control">
          <select
            value={value ?? ""}
            onChange={(e) => handleSelectChange(e.target.value)}
            data-testid="settings-terminal-font-family"
            aria-label="Terminal font family"
          >
            <option value="">Default (system cascade)</option>
            <optgroup label="Bundled">
              {BUNDLED_FONTS.map((f) => (
                <option key={f.family} value={f.family}>
                  {f.label}
                </option>
              ))}
            </optgroup>
            {systemFonts && systemFonts.length > 0 && (
              <optgroup label="System">
                {systemFonts.map((family) => (
                  <option key={`sys:${family}`} value={family}>
                    {family}
                  </option>
                ))}
              </optgroup>
            )}
            {!valueIsKnown && value && (
              <optgroup label="Current">
                <option value={value}>{value} (saved)</option>
              </optgroup>
            )}
          </select>
        </div>
      </div>
      <div className="settings-row">
        <span />
        <div className="settings-row-control">
          <button
            type="button"
            className="link"
            disabled={!canQueryLocalFonts}
            onClick={() => void loadSystemFonts()}
            title={
              canQueryLocalFonts
                ? "List your OS fonts so you can pick one from the dropdown."
                : "Your webview doesn't expose the Local Font Access API."
            }
            data-testid="settings-terminal-font-load-system"
          >
            {systemFonts === null
              ? "Load system fonts…"
              : `Refresh (${systemFonts.length} loaded)`}
          </button>
        </div>
      </div>
      {systemFontsError && (
        <p className="settings-section-hint">
          Couldn't load system fonts: {systemFontsError}
        </p>
      )}
      <p className="settings-section-hint">
        Bundled coding fonts ship with the app. Pick "Load system fonts"
        to add your OS fonts to the list — the browser will ask for
        permission the first time.
      </p>
    </>
  );
}

function Toggle({
  testid,
  label,
  checked,
  onChange,
}: {
  testid: string;
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className="settings-toggle">
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        data-testid={testid}
      />
      <span>{label}</span>
    </label>
  );
}

function PermissionBadge({ state }: { state: PermissionState }) {
  const cls =
    state === "granted"
      ? "badge badge-ok"
      : state === "denied"
        ? "badge badge-err"
        : "badge badge-warn";
  const label =
    state === "granted"
      ? "granted"
      : state === "denied"
        ? "denied"
        : state === "default"
          ? "not set"
          : "checking…";
  return (
    <span className={cls} data-testid="settings-permission-state">
      {label}
    </span>
  );
}
