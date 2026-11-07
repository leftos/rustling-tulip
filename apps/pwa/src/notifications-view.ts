import type { ConnectionStateName } from "./connection-machine";
import { homeVisible } from "./home-screen";
import type { ClientMessage } from "./protocol";
import {
  deviceRead,
  foldError,
  foldStatus,
  markFailed,
  markOffline,
  openCheck,
  panelOpen,
  permissionAnswered,
  pushScreen,
  pushState,
  setSwitch,
  subscribed,
  turnOff,
  turnOn,
  unsubscribed,
  type PushDevice,
  type PushEffect,
  type PushScreen,
  type PushState,
  type PushStep,
  type PushSwitch,
  type PushSwitchName,
} from "./push-model";
import type { PushPort } from "./push-port";
import type { Connection } from "./socket";

/**
 * The Notifications panel: a section shown over Home and opened from Home's
 * Notifications button. It folds the connection's notices into the push model,
 * runs the browser work the model asks for through the port, and paints what
 * the model makes. The check on app open runs once on each ready, paired
 * connection and never prompts or subscribes: iOS needs a tap for both. The
 * button and the panel follow Home's visibility, so neither stands over the
 * pairing screen or a session.
 */
export function mountNotificationsView(
  section: HTMLElement,
  button: HTMLElement | null,
  connection: Connection,
  port: PushPort,
): () => void {
  let state: PushState = pushState(deviceNow());
  let opened = false;
  let status: ConnectionStateName = "idle";
  let shown = false;

  function deviceNow(): PushDevice {
    // A browser without the push API has no `Notification` or `PushManager` to
    // read, so its device is the support flag alone.
    if (!port.supported()) {
      return { supported: false, installed: false, permission: "default", keys: null };
    }
    return {
      supported: true,
      installed: port.installed(),
      permission: port.permission(),
      keys: null,
    };
  }

  async function readDevice(): Promise<PushDevice> {
    const device = deviceNow();
    if (!device.supported) {
      return device;
    }
    return { ...device, keys: await port.current() };
  }

  function newRequestId(): string {
    return crypto.randomUUID();
  }

  function send(message: ClientMessage): void {
    if (connection.send(message)) {
      return;
    }
    state = markOffline(state);
  }

  function apply(step: PushStep): void {
    state = step.state;
    if (step.message !== null) {
      send(step.message);
    }
    if (step.effects.length > 0) {
      void runEffects(step.effects);
    }
    update();
  }

  async function runEffects(effects: readonly PushEffect[]): Promise<void> {
    for (const effect of effects) {
      try {
        switch (effect.kind) {
          case "ask-permission": {
            const permission = await port.requestPermission();
            apply(permissionAnswered(state, permission));
            break;
          }
          case "subscribe": {
            const keys = await port.subscribe(effect.vapidPublicKey);
            apply(subscribed(state, keys, newRequestId));
            break;
          }
          case "unsubscribe": {
            await port.unsubscribe();
            state = unsubscribed(state);
            update();
            break;
          }
        }
      } catch (error) {
        state = markFailed(state, textOf(error));
        update();
        return;
      }
    }
  }

  function update(): void {
    section.hidden = !opened;
    if (opened) {
      paint(pushScreen(state));
    }
  }

  function paint(screen: PushScreen): void {
    const nodes: HTMLElement[] = [];
    const title = element("h2", "notifications-title");
    title.textContent = screen.heading;
    nodes.push(title);
    if (screen.message !== null) {
      const text = element("p", "notifications-text");
      text.textContent = screen.message;
      nodes.push(text);
    }
    const rows: HTMLElement[] = [];
    const allow = screen.allow;
    if (allow !== null) {
      rows.push(toggle(allow, () => openChanged(allow)));
    }
    const needsYou = screen.needsYou;
    if (needsYou !== null) {
      rows.push(switchRow(needsYou, "needsYou"));
    }
    const finished = screen.finished;
    if (finished !== null) {
      rows.push(switchRow(finished, "finished"));
    }
    const holdAtDesk = screen.holdAtDesk;
    if (holdAtDesk !== null) {
      rows.push(switchRow(holdAtDesk, "holdAtDesk"));
    }
    if (rows.length > 0) {
      const switches = element("div", "notifications-switches");
      switches.append(...rows);
      nodes.push(switches);
    }
    if (screen.error !== null) {
      const failure = element("p", "notifications-error");
      failure.textContent = screen.error;
      nodes.push(failure);
    }
    const done = element("button", "notifications-done");
    done.type = "button";
    done.textContent = screen.done;
    done.addEventListener("click", close);
    nodes.push(done);
    section.replaceChildren(...nodes);
  }

  function openChanged(allow: PushSwitch): void {
    apply(allow.on ? turnOff(state, newRequestId) : turnOn(state));
  }

  function switchRow(view: PushSwitch, which: PushSwitchName): HTMLElement {
    return toggle(view, () => {
      apply(setSwitch(state, which, !view.on, newRequestId));
    });
  }

  function toggle(view: PushSwitch, onChange: () => void): HTMLElement {
    const row = element("label", "notifications-switch");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.className = "notifications-toggle";
    box.checked = view.on;
    box.disabled = !view.enabled;
    box.addEventListener("change", onChange);
    const name = element("span", "notifications-switch-label");
    name.textContent = view.label;
    row.append(box, name);
    return row;
  }

  function openPanel(): void {
    opened = true;
    update();
    void refresh();
  }

  async function refresh(): Promise<void> {
    try {
      const device = await readDevice();
      state = deviceRead(state, device);
      apply(panelOpen(state, newRequestId));
    } catch (error) {
      state = markFailed(state, textOf(error));
      update();
    }
  }

  function close(): void {
    opened = false;
    update();
  }

  async function startCheck(): Promise<void> {
    try {
      const device = await readDevice();
      state = deviceRead(state, device);
      apply(openCheck(state, newRequestId));
    } catch (error) {
      state = markFailed(state, textOf(error));
      update();
    }
  }

  /** The button and the panel follow Home; the panel closes as soon as Home goes. */
  function sync(): void {
    const visible = homeVisible(status, shown, connection.paired());
    shown = shown || visible;
    if (button !== null) {
      button.hidden = !visible;
    }
    if (!visible) {
      opened = false;
    }
    update();
  }

  function textOf(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
  }

  function element<K extends keyof HTMLElementTagNameMap>(
    tag: K,
    className: string,
  ): HTMLElementTagNameMap[K] {
    const node = document.createElement(tag);
    node.className = className;
    return node;
  }

  if (button !== null) {
    button.addEventListener("click", openPanel);
  }
  window.addEventListener("popstate", close);

  const unsubscribe = connection.subscribe((notice) => {
    if (notice.kind === "status") {
      status = notice.state;
      if (status === "ready" && connection.paired()) {
        // Every `ready` is a fresh start: the daemon resends its whole state,
        // so nothing from the old connection is kept.
        state = pushState(deviceNow());
        sync();
        void startCheck();
        return;
      }
      state = markOffline(state);
      sync();
      return;
    }
    if (notice.kind === "message") {
      if (notice.message.type === "push_status") {
        apply(foldStatus(state, notice.message, newRequestId));
      } else if (notice.message.type === "error") {
        apply(foldError(state, notice.message));
      }
    }
  });
  sync();

  return () => {
    unsubscribe();
    window.removeEventListener("popstate", close);
  };
}
