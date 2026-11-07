import type { ConnectionStateName } from "./connection-machine";
import { isPairingCode, type PairingPhase } from "./pairing";
import {
  CAMERA_OFF,
  cameraStep,
  pairingScreen,
  shouldSendScan,
  type CameraEvent,
  type CameraState,
  type CameraTrack,
  type CodeEntry,
  type Screen,
} from "./pairing-screen";
import type { CodeScanner } from "./scanner";
import type { Connection } from "./socket";

/** The camera error names that mean this app may not use it at all. */
const DENIED_ERRORS: ReadonlySet<string> = new Set([
  "NotAllowedError",
  "PermissionDeniedError",
  "SecurityError",
]);

/**
 * The pairing screen over one connection: it builds its nodes once, renders
 * each [`Screen`] the model makes, and wires the tap, the typed code and the
 * scanner to the connection. It holds no logic beyond that and the camera's
 * own state; the texts all come from the model.
 */
export function mountPairingView(
  container: HTMLElement,
  connection: Connection,
  scanner: CodeScanner,
): () => void {
  const title = element("h2", "pairing-title");
  const body = element("p", "pairing-body");
  const error = element("p", "pairing-error");
  const ask = element("button", "pairing-ask");
  ask.type = "button";
  const video = document.createElement("video");
  video.className = "pairing-video";
  video.playsInline = true;
  video.muted = true;
  video.autoplay = true;
  const note = element("p", "pairing-camera-note");
  const scannerArea = element("div", "pairing-scanner");
  scannerArea.append(video, note);
  const label = element("label", "pairing-label");
  label.htmlFor = "pairing-code";
  const input = document.createElement("input");
  input.id = "pairing-code";
  input.className = "pairing-code";
  input.type = "text";
  const pair = element("button", "pairing-pair");
  pair.type = "submit";
  const entry = element("form", "pairing-entry");
  entry.append(label, input, pair);
  container.append(title, body, error, ask, scannerArea, entry);

  let status: ConnectionStateName = "idle";
  let track: CameraTrack = CAMERA_OFF;
  /** The phase a start was last tried in, so a refused camera is not retried there. */
  let triedFor: PairingPhase | null = null;
  /** The phase a code was sent in: the camera stays off until that changes. */
  let sentFor: PairingPhase | null = null;
  /** The code a redeem went out for, while the daemon has not answered it. */
  let awaiting: string | null = null;
  /** The code the daemon refused last, for as long as this request stands. */
  let lastRefused: string | null = null;

  function update(): void {
    const pairing = connection.pairingState();
    if (sentFor !== null && (pairing.phase !== sentFor || pairing.error !== null)) {
      sentFor = null;
    }
    const screen = pairingScreen(pairing, status, track.state);
    paint(screen);
    syncCamera(screen, pairing.phase);
  }

  function paint(screen: Screen): void {
    container.hidden = !screen.visible;
    title.textContent = screen.title;
    body.textContent = screen.body;
    showText(error, screen.error);
    ask.textContent = screen.button?.label ?? "";
    ask.hidden = screen.button === null;
    ask.disabled = screen.button !== null && !screen.button.enabled;
    scannerArea.hidden = !screen.scanner;
    showText(note, screen.cameraNote);
    video.hidden = screen.cameraNote !== null;
    paintEntry(screen.entry);
  }

  function paintEntry(fields: CodeEntry | null): void {
    entry.hidden = fields === null;
    if (fields === null) {
      return;
    }
    label.textContent = fields.label;
    input.inputMode = fields.inputMode;
    input.autocomplete = fields.autoComplete;
    input.maxLength = fields.maxLength;
    pair.textContent = fields.button.label;
  }

  function syncCamera(screen: Screen, phase: PairingPhase): void {
    if (!screen.camera || phase === sentFor) {
      stopCamera();
      return;
    }
    if (track.state === "starting" || track.state === "on" || triedFor === phase) {
      return;
    }
    triedFor = phase;
    track = cameraStep(track, { type: "start" });
    const attempt = track.attempt;
    update();
    void scanner.start(video, onScanned).then(
      () => {
        settle({ type: "opened", attempt });
      },
      (cause: unknown) => {
        settle({ type: "failed", attempt, state: cameraFailure(cause) });
      },
    );
  }

  function settle(event: CameraEvent): void {
    const next = cameraStep(track, event);
    if (next !== track) {
      track = next;
      update();
    }
  }

  function stopCamera(): void {
    if (track.state !== "starting" && track.state !== "on") {
      return;
    }
    track = cameraStep(track, { type: "stop" });
    triedFor = null;
    scanner.stop();
  }

  /** A code the scanner read: sent unless it is the one the daemon just refused. */
  function onScanned(code: string): boolean {
    if (!shouldSendScan(code, lastRefused)) {
      return false;
    }
    send(code);
    return true;
  }

  /** Send a code, typed or scanned; the camera sleeps until the phase moves on. */
  function send(raw: string): void {
    const code = raw.trim();
    if (!isPairingCode(code)) {
      connection.redeem(code);
      update();
      return;
    }
    sentFor = connection.pairingState().phase;
    awaiting = code;
    connection.redeem(code);
    stopCamera();
    input.value = "";
    update();
  }

  ask.addEventListener("click", () => {
    connection.askToPair();
  });
  entry.addEventListener("submit", (event) => {
    event.preventDefault();
    send(input.value);
  });
  const unsubscribe = connection.subscribe((notice) => {
    if (notice.kind === "status") {
      status = notice.state;
      update();
      return;
    }
    if (notice.kind === "message") {
      if (notice.message.type === "device_pair_failed" && awaiting !== null) {
        lastRefused = awaiting;
        awaiting = null;
      }
      return;
    }
    if (notice.pairing.phase === "asking" || notice.pairing.phase === "paired") {
      awaiting = null;
      lastRefused = null;
    }
    update();
  });
  update();

  return () => {
    unsubscribe();
    stopCamera();
    scanner.stop();
  };
}

/** The camera's state, from the error the browser refused the stream with. */
function cameraFailure(cause: unknown): CameraState {
  return DENIED_ERRORS.has(errorName(cause)) ? "denied" : "unavailable";
}

function errorName(cause: unknown): string {
  if (typeof cause !== "object" || cause === null || !("name" in cause)) {
    return "";
  }
  const name: unknown = cause.name;
  return typeof name === "string" ? name : "";
}

function showText(node: HTMLElement, text: string | null): void {
  node.hidden = text === null;
  node.textContent = text ?? "";
}

function element<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.className = className;
  return node;
}
