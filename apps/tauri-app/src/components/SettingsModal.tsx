import { useCallback, useEffect, useRef, useState } from "react";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import type { CodexSandbox, PermissionMode } from "../types";
import { logToFile } from "../utils/logger";
import { useEscape, useFocusReturn } from "../utils/a11y";
import { saveSettings, type Settings } from "../utils/settings";

interface Props {
  settings: Settings;
  onClose: () => void;
}

type PermissionState = "granted" | "denied" | "default" | "unknown";

/// Settings modal — a thin localStorage-backed configuration surface. Wired
/// in iter 49 to unblock several audit items (notification re-request,
/// persisted skip-perms / sidebar-view defaults). State persistence runs
/// through `saveSettings` in `utils/settings.ts`, which writes localStorage
/// AND fires a `rt:settings-changed` window event so cross-component
/// subscribers (notification gate, Sidebar, SpawnDialog) re-read on every
/// write without prop-drilling.
export default function SettingsModal({ settings, onClose }: Props) {
  const closeRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onClose);
  useFocusReturn();

  // Edit-in-place: we mutate a local copy and call `saveSettings` on each
  // change so the user sees instant feedback (toggles flip, dropdowns
  // commit) without a separate Save / Apply button. The localStorage write
  // is cheap. Pre-iter-49 there was no live-sync requirement — first iter
  // intentionally keeps it simple.
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
              label="Skip permissions / yolo by default"
              checked={settings.spawn.skip_permissions_default}
              onChange={(v) =>
                update((s) => ({
                  ...s,
                  spawn: { ...s.spawn, skip_permissions_default: v },
                }))
              }
            />
            <div className="settings-row">
              <span>Default permission mode (claude)</span>
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
                    ? "Ignored while 'skip permissions' is on"
                    : ""
                }
              >
                <option value="">(none — claude's default)</option>
                <option value="default">default</option>
                <option value="accept_edits">accept_edits</option>
                <option value="bypass_permissions">bypass_permissions</option>
                <option value="plan">plan</option>
              </select>
            </div>
            <div className="settings-row">
              <span>Default sandbox (codex)</span>
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
                    ? "Ignored while 'yolo' is on"
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
