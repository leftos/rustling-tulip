import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { getAutostart, setAutostart, type DaemonClient } from "../api";
import type { CodexSandbox, PermissionMode } from "../types";
import { logToFile } from "../utils/logger";
import { useEscape, useFocusReturn } from "../utils/a11y";
import { saveSettings, type Settings } from "../utils/settings";
import { resolveAppearanceLayers } from "../utils/appearance";
import { AppearanceFields } from "./AppearanceEditor";
import Icon from "./Icon";

interface Props {
  settings: Settings;
  onClose: () => void;
  /// Daemon connection for sending `set_worktrees_root` mutations from
  /// the Worktrees panel. `null` while disconnected — the panel disables
  /// its mutation buttons in that state.
  client: DaemonClient | null;
  /// Active worktrees root + override flag, mirrored from the daemon's
  /// `worktrees_root_changed` broadcast. `null` only before the initial
  /// state push lands; the panel renders a placeholder until then.
  worktreesRoot: { path: string; isOverride: boolean } | null;
  /// Open the worktrees-management modal. Owned by the parent so it
  /// lives at the App level (matches the SettingsModal own placement).
  onOpenWorktreesManager: () => void;
  /// Opt-in LAN access status, mirrored from the daemon's `lan_status`
  /// broadcast. `null` before the initial state push lands.
  lanStatus: {
    enabled: boolean;
    port: number;
    fingerprint: string | null;
    addresses: string[];
  } | null;
  /// The local daemon's auth token, for assembling the remote connection
  /// code. `null` until the first connect.
  daemonToken: string | null;
}

type PermissionState = "granted" | "denied" | "default" | "unknown";

type SettingsTabKey =
  | "general"
  | "notifications"
  | "spawn"
  | "worktrees"
  | "lan"
  | "appearance"
  | "title";

interface SettingsTabDef {
  key: SettingsTabKey;
  label: string;
}

const SETTINGS_TABS: SettingsTabDef[] = [
  { key: "general", label: "General" },
  { key: "notifications", label: "Notifications" },
  { key: "spawn", label: "Spawn defaults" },
  { key: "worktrees", label: "Worktrees" },
  { key: "lan", label: "Remote access" },
  { key: "appearance", label: "Appearance" },
  { key: "title", label: "App Title" },
];

/// Settings modal — a thin localStorage-backed configuration surface.
/// State persistence runs through `saveSettings` in `utils/settings.ts`,
/// which writes localStorage AND fires a `rt:settings-changed` window event
/// so cross-component subscribers re-read on every write without prop-drilling.
export default function SettingsModal({
  settings,
  onClose,
  client,
  worktreesRoot,
  onOpenWorktreesManager,
  lanStatus,
  daemonToken,
}: Props) {
  const closeRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onClose);
  useFocusReturn();

  const [activeTab, setActiveTab] = useState<SettingsTabKey>("general");

  // Edit-in-place: we mutate a local copy and call `saveSettings` on each
  // change so the user sees instant feedback without a separate Save /
  // Apply button.
  const update = useCallback(
    (mut: (s: Settings) => Settings) => {
      saveSettings(mut(settings));
    },
    [settings],
  );

  // Focus the Close button by default — Settings is a non-destructive
  // surface and we don't have a single "primary" action to autofocus.
  // Close is the safest landing point for keyboard users.
  useEffect(() => {
    closeRef.current?.focus();
  }, []);

  return (
    <div
      className="modal-backdrop modal-backdrop-preview"
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
            <Icon name="close" />
          </button>
        </header>
        <div className="settings-layout">
          <nav
            className="settings-tablist"
            role="tablist"
            aria-label="Settings sections"
          >
            {SETTINGS_TABS.map((tab) => (
              <button
                key={tab.key}
                type="button"
                role="tab"
                id={`settings-tab-${tab.key}`}
                aria-controls={`settings-panel-${tab.key}`}
                aria-selected={activeTab === tab.key}
                className={
                  activeTab === tab.key
                    ? "settings-tab settings-tab-active"
                    : "settings-tab"
                }
                onClick={() => setActiveTab(tab.key)}
                data-testid={`settings-tab-${tab.key}`}
              >
                {tab.label}
              </button>
            ))}
          </nav>
          <div
            className="modal-body settings-body"
            role="tabpanel"
            id={`settings-panel-${activeTab}`}
            aria-labelledby={`settings-tab-${activeTab}`}
          >
            {activeTab === "general" && (
              <GeneralPanel settings={settings} update={update} />
            )}
            {activeTab === "notifications" && (
              <NotificationsPanel settings={settings} update={update} />
            )}
            {activeTab === "spawn" && (
              <SpawnPanel settings={settings} update={update} />
            )}
            {activeTab === "worktrees" && (
              <WorktreesPanel
                client={client}
                worktreesRoot={worktreesRoot}
                onOpenManager={onOpenWorktreesManager}
              />
            )}
            {activeTab === "lan" && (
              <LanPanel
                client={client}
                lanStatus={lanStatus}
                daemonToken={daemonToken}
              />
            )}
            {activeTab === "appearance" && (
              <AppearanceFields
                value={settings.appearance}
                inherited={resolveAppearanceLayers(null, null, null)}
                inheritLabel="Use built-in"
                onChange={(appearance) =>
                  update((s) => ({
                    ...s,
                    appearance,
                  }))
                }
              />
            )}
            {activeTab === "title" && (
              <TitlePanel settings={settings} update={update} />
            )}
          </div>
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

interface PanelProps {
  settings: Settings;
  update: (mut: (s: Settings) => Settings) => void;
}

function GeneralPanel({ settings, update }: PanelProps) {
  return (
    <>
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
              aria-checked={settings.sidebar.default_view === "container"}
              className={
                settings.sidebar.default_view === "container" ? "active" : ""
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
        data-testid="settings-section-terminal"
      >
        <h3>Terminal behavior</h3>
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
    </>
  );
}

function NotificationsPanel({ settings, update }: PanelProps) {
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

  return (
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
  );
}

function SpawnPanel({ settings, update }: PanelProps) {
  return (
    <section
      className="settings-section"
      data-testid="settings-section-spawn"
    >
      <h3>Spawn defaults</h3>
      <p className="settings-section-hint">
        Pre-fill these in the spawn dialog. You can override on every spawn.
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
              value === "" ? null : (value as PermissionMode);
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
              value === "" ? null : (value as CodexSandbox);
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
  );
}

function TitlePanel({ settings, update }: PanelProps) {
  const preview = useMemo(() => {
    const busyPrefix = settings.title.show_busy_count ? "(1/3) " : "";
    const suffix = settings.title.show_product_suffix
      ? " — rustling-tulip"
      : "";
    return `${busyPrefix}Tab name${suffix}`;
  }, [settings.title]);

  return (
    <section
      className="settings-section"
      data-testid="settings-section-title"
    >
      <h3>App title</h3>
      <p className="settings-section-hint">
        Controls what the OS window title bar shows for the main window.
        Pop-out windows always show just the tab / session name.
      </p>
      <Toggle
        testid="settings-title-busy-count"
        label="Show busy/total terminal count"
        checked={settings.title.show_busy_count}
        onChange={(v) =>
          update((s) => ({
            ...s,
            title: { ...s.title, show_busy_count: v },
          }))
        }
      />
      <p className="settings-section-hint">
        Renders <code>(M/N)</code> before the tab name where M is the number
        of non-idle terminals in the active tab and N is the total terminal
        count. Hidden when the tab has no terminals.
      </p>
      <Toggle
        testid="settings-title-product-suffix"
        label={"Append “ — rustling-tulip” suffix"}
        checked={settings.title.show_product_suffix}
        onChange={(v) =>
          update((s) => ({
            ...s,
            title: { ...s.title, show_product_suffix: v },
          }))
        }
      />
      <p className="settings-section-hint">
        Keeps OS taskbars grouping rustling-tulip windows together. Turn off
        for a tighter title.
      </p>
      <div className="settings-row">
        <span>Preview</span>
        <code
          className="settings-title-preview"
          data-testid="settings-title-preview"
        >
          {preview || "(empty)"}
        </code>
      </div>
    </section>
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

interface WorktreesPanelProps {
  client: DaemonClient | null;
  worktreesRoot: { path: string; isOverride: boolean } | null;
  onOpenManager: () => void;
}

/// Worktrees-root setting + entry point for the management modal. The
/// daemon is the source of truth for the active root (it pushes
/// `worktrees_root_changed` on connect and on every override mutation);
/// this panel just edits a local draft and pushes `set_worktrees_root`
/// on Save. The draft re-syncs whenever the daemon broadcast updates so
/// concurrent changes from another window propagate without manual
/// refresh.
interface LanPanelProps {
  client: DaemonClient | null;
  lanStatus: {
    enabled: boolean;
    port: number;
    fingerprint: string | null;
    addresses: string[];
  } | null;
  daemonToken: string | null;
}

const LAN_CODE_VERSION = 1;
const DEFAULT_LAN_PORT = 8787;

/// Encode the host's LAN connection details as a single base64url blob the
/// remote client pastes once. Carries the pinned cert fingerprint + auth
/// token, so it grants full control — treated as a secret in the UI copy.
function buildConnectionCode(
  host: string,
  port: number,
  token: string,
  fingerprint: string,
): string {
  const json = JSON.stringify({
    v: LAN_CODE_VERSION,
    host,
    port,
    token,
    fp: fingerprint,
  });
  return btoa(json)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

/// Remote-access (LAN) settings: toggle the opt-in TLS listener, pick which
/// of the host's addresses to advertise, and copy the connection code a remote
/// client pastes to pair. The daemon is the source of truth (`lan_status`);
/// this panel just sends `configure_lan` and renders the broadcast back.
function LanPanel({ client, lanStatus, daemonToken }: LanPanelProps) {
  const enabled = lanStatus?.enabled ?? false;
  const addresses = lanStatus?.addresses ?? [];
  const [portDraft, setPortDraft] = useState<string>(
    String(lanStatus?.port ?? DEFAULT_LAN_PORT),
  );
  const [selectedAddr, setSelectedAddr] = useState<string>("");
  const [copied, setCopied] = useState(false);
  // Host-side "start daemon on login" (Windows only). `null` until the initial
  // read resolves or if autostart is unsupported on this platform.
  const [autostartEnabled, setAutostartEnabled] = useState<boolean | null>(
    null,
  );
  const [autostartBusy, setAutostartBusy] = useState(false);

  useEffect(() => {
    void getAutostart()
      .then(setAutostartEnabled)
      .catch(() => setAutostartEnabled(null));
  }, []);

  const onToggleAutostart = useCallback(() => {
    if (autostartEnabled === null || autostartBusy) return;
    const next = !autostartEnabled;
    setAutostartBusy(true);
    void setAutostart(next)
      .then(() => setAutostartEnabled(next))
      .catch((err: unknown) => {
        logToFile("warn", `setAutostart failed: ${String(err)}`);
      })
      .finally(() => setAutostartBusy(false));
  }, [autostartEnabled, autostartBusy]);

  // Re-sync the port input when the daemon's status changes.
  useEffect(() => {
    setPortDraft(String(lanStatus?.port ?? DEFAULT_LAN_PORT));
  }, [lanStatus?.port]);
  // Default the address selection to the first detected address, keeping a
  // valid prior choice if it still exists.
  useEffect(() => {
    setSelectedAddr((prev) =>
      prev && addresses.includes(prev) ? prev : (addresses[0] ?? ""),
    );
  }, [addresses]);

  const parsedPort = Number.parseInt(portDraft, 10);
  const portValid =
    Number.isInteger(parsedPort) && parsedPort >= 1 && parsedPort <= 65535;

  const onToggle = useCallback(() => {
    if (!client || !portValid) return;
    client.send({
      type: "configure_lan",
      enabled: !enabled,
      port: parsedPort,
    });
  }, [client, enabled, parsedPort, portValid]);

  const onApplyPort = useCallback(() => {
    if (!client || !portValid || !enabled) return;
    client.send({ type: "configure_lan", enabled: true, port: parsedPort });
  }, [client, enabled, parsedPort, portValid]);

  const connectionCode = useMemo(() => {
    if (!enabled || !daemonToken || !lanStatus?.fingerprint || !selectedAddr) {
      return null;
    }
    return buildConnectionCode(
      selectedAddr,
      lanStatus.port,
      daemonToken,
      lanStatus.fingerprint,
    );
  }, [enabled, daemonToken, lanStatus, selectedAddr]);

  const onCopy = useCallback(() => {
    if (!connectionCode) return;
    void navigator.clipboard
      .writeText(connectionCode)
      .then(() => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1500);
      })
      .catch((err: unknown) => {
        logToFile(
          "warn",
          `settings: copy connection code failed: ${String(err)}`,
        );
      });
  }, [connectionCode]);

  const disabled = !client;
  return (
    <section className="settings-section" data-testid="settings-section-lan">
      <h3>Remote access (LAN)</h3>
      <p className="settings-section-hint">
        Let another machine on your local network connect to this daemon and
        control its shells over an encrypted (TLS) connection. Off by default —
        the daemon stays loopback-only until you enable this.
      </p>
      <p className="settings-section-hint">
        ⚠ The connection code below grants full control of this machine's
        shells — it can run commands here. Only share it with devices you trust
        on a network you trust.
      </p>
      <div className="settings-row">
        <label htmlFor="lan-port-input">Port</label>
        <input
          id="lan-port-input"
          type="text"
          inputMode="numeric"
          value={portDraft}
          onChange={(e) => setPortDraft(e.target.value)}
          spellCheck={false}
          autoComplete="off"
          disabled={disabled}
          data-testid="settings-lan-port-input"
        />
        {enabled && (
          <button
            type="button"
            onClick={onApplyPort}
            disabled={disabled || !portValid || parsedPort === lanStatus?.port}
          >
            Apply
          </button>
        )}
      </div>
      {!portValid && (
        <p className="settings-section-hint">
          Port must be a number between 1 and 65535.
        </p>
      )}
      <div className="settings-row">
        <button
          type="button"
          onClick={onToggle}
          disabled={disabled || !portValid}
          data-testid="settings-lan-toggle"
        >
          {enabled ? "Disable LAN access" : "Enable LAN access"}
        </button>
        <span className="settings-section-hint">
          {lanStatus === null
            ? "(daemon not connected)"
            : enabled
              ? "Enabled"
              : "Disabled"}
        </span>
      </div>
      {autostartEnabled !== null && (
        <div className="settings-row">
          <button
            type="button"
            onClick={onToggleAutostart}
            disabled={autostartBusy}
            data-testid="settings-autostart-toggle"
          >
            {autostartEnabled
              ? "Don't start daemon on login"
              : "Start daemon on login"}
          </button>
          <span className="settings-section-hint">
            {autostartEnabled
              ? "The daemon launches after you sign in, so this machine stays reachable without opening the app."
              : "Make this machine reachable after a reboot without opening the app."}
          </span>
        </div>
      )}
      {enabled && (
        <>
          {addresses.length > 1 && (
            <div className="settings-row">
              <label htmlFor="lan-addr-select">This machine's address</label>
              <select
                id="lan-addr-select"
                value={selectedAddr}
                onChange={(e) => setSelectedAddr(e.target.value)}
                data-testid="settings-lan-addr-select"
              >
                {addresses.map((a) => (
                  <option key={a} value={a}>
                    {a}
                  </option>
                ))}
              </select>
            </div>
          )}
          {addresses.length === 1 && (
            <p className="settings-section-hint">
              This machine: <code>{addresses[0]}</code>
            </p>
          )}
          {addresses.length === 0 && (
            <p className="settings-section-hint">
              No LAN address detected — check your network connection.
            </p>
          )}
          {lanStatus?.fingerprint && (
            <p className="settings-section-hint">
              Certificate fingerprint:{" "}
              <code>{lanStatus.fingerprint.slice(0, 32)}…</code>
            </p>
          )}
          {connectionCode ? (
            <div className="settings-row">
              <label htmlFor="lan-conn-code">Connection code</label>
              <input
                id="lan-conn-code"
                type="text"
                value={connectionCode}
                readOnly
                onFocus={(e) => e.currentTarget.select()}
                spellCheck={false}
                data-testid="settings-lan-connection-code"
              />
              <button
                type="button"
                onClick={onCopy}
                data-testid="settings-lan-copy"
              >
                {copied ? "Copied!" : "Copy"}
              </button>
            </div>
          ) : (
            <p className="settings-section-hint">
              Connection code unavailable — waiting for the certificate and a
              detected address…
            </p>
          )}
          <p className="settings-section-hint">
            On the other machine, open rustling-tulip, choose “Add remote”, and
            paste this code.
          </p>
        </>
      )}
    </section>
  );
}

function WorktreesPanel({
  client,
  worktreesRoot,
  onOpenManager,
}: WorktreesPanelProps) {
  const isOverride = worktreesRoot?.isOverride ?? false;
  const currentPath = worktreesRoot?.path ?? null;
  const [draft, setDraft] = useState<string>(isOverride ? currentPath ?? "" : "");
  const [savingState, setSavingState] = useState<"idle" | "saving" | "saved">(
    "idle",
  );

  // Re-sync the input when the daemon's known value changes. Only echo
  // back the override path — leaving the input empty when the daemon is
  // on default makes Save mean "set this as override" rather than
  // "set the default path as override".
  useEffect(() => {
    setDraft(isOverride ? currentPath ?? "" : "");
    setSavingState("idle");
  }, [isOverride, currentPath]);

  const onBrowse = useCallback(() => {
    void (async () => {
      try {
        const seed = draft || currentPath;
        const result = await openDialog({
          directory: true,
          multiple: false,
          title: "Choose worktrees root",
          ...(seed ? { defaultPath: seed } : {}),
        });
        if (typeof result === "string" && result.length > 0) {
          setDraft(result);
        }
      } catch (err) {
        logToFile("warn", `settings: worktrees Browse threw: ${String(err)}`);
      }
    })();
  }, [draft, currentPath]);

  const onSave = useCallback(() => {
    if (!client) return;
    const trimmed = draft.trim();
    setSavingState("saving");
    client.send({
      type: "set_worktrees_root",
      path: trimmed.length === 0 ? null : trimmed,
    });
    // No request/reply contract — the daemon's `worktrees_root_changed`
    // broadcast is the confirmation, which fires the useEffect above and
    // resets savingState back to "idle". We tag "saved" optimistically
    // so the user sees feedback even on transient broadcast delays.
    setSavingState("saved");
  }, [client, draft]);

  const onReset = useCallback(() => {
    if (!client) return;
    setSavingState("saving");
    client.send({ type: "set_worktrees_root", path: null });
    setDraft("");
  }, [client]);

  const disabled = !client;
  const draftMatchesCurrent = isOverride
    ? draft.trim() === (currentPath ?? "")
    : draft.trim().length === 0;
  return (
    <section
      className="settings-section"
      data-testid="settings-section-worktrees"
    >
      <h3>Worktrees root</h3>
      <p className="settings-section-hint">
        Per-session git worktrees land under this directory. Changes take
        effect for new sessions only — sessions already spawned keep
        their existing worktree paths.
      </p>
      <div className="settings-row">
        <label htmlFor="worktrees-root-input">Path</label>
        <input
          id="worktrees-root-input"
          type="text"
          value={draft}
          onChange={(e) => {
            setDraft(e.target.value);
            setSavingState("idle");
          }}
          placeholder={isOverride ? "" : currentPath ?? "(daemon not connected)"}
          spellCheck={false}
          autoComplete="off"
          disabled={disabled}
          data-testid="settings-worktrees-root-input"
        />
        <button
          type="button"
          onClick={onBrowse}
          disabled={disabled}
          data-testid="settings-worktrees-root-browse"
        >
          Browse…
        </button>
      </div>
      <p className="settings-section-hint">
        Active: <code>{currentPath ?? "(daemon not connected)"}</code>
        {isOverride
          ? " — user override"
          : " — default (env or platform fallback)"}
      </p>
      <div className="settings-row">
        <button
          type="button"
          className="primary"
          onClick={onSave}
          disabled={disabled || draftMatchesCurrent}
          data-testid="settings-worktrees-root-save"
        >
          {savingState === "saved" && draftMatchesCurrent ? "Saved" : "Save"}
        </button>
        <button
          type="button"
          onClick={onReset}
          disabled={disabled || !isOverride}
          data-testid="settings-worktrees-root-reset"
        >
          Reset to default
        </button>
        <button
          type="button"
          onClick={onOpenManager}
          disabled={disabled}
          data-testid="settings-worktrees-open-manager"
        >
          Manage worktrees…
        </button>
      </div>
    </section>
  );
}
