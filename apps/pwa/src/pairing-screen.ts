import type { ConnectionStateName } from "./connection-machine";
import type { PairingPhase, PairingState } from "./pairing";

/**
 * What the pairing screen shows: a pure view model over the pairing model, the
 * connection's state and the camera's, so the texts and the visible parts are
 * testable without a DOM. `pairing-view.ts` only renders it.
 */

/** What the camera is doing, as the view knows it. */
export type CameraState = "off" | "starting" | "on" | "denied" | "unavailable";

/** A button the screen offers; `enabled` false means it shows, but does nothing. */
export interface ScreenButton {
  readonly label: string;
  readonly enabled: boolean;
}

/** The typed code entry, with the attributes that shape its input. */
export interface CodeEntry {
  readonly label: string;
  readonly inputMode: "numeric";
  readonly autoComplete: "one-time-code";
  readonly maxLength: number;
  readonly button: ScreenButton;
}

/** Everything the pairing screen shows, and whether the camera runs. */
export interface Screen {
  /** False once the phone is paired and connected: the page's own content stands. */
  readonly visible: boolean;
  readonly title: string;
  readonly body: string;
  /** The phase's one button, or `null` for none. */
  readonly button: ScreenButton | null;
  /** Whether the scanner area shows. */
  readonly scanner: boolean;
  /** The typed entry, or `null` when it does not show. */
  readonly entry: CodeEntry | null;
  /** What stands in for the preview when the camera cannot run. */
  readonly cameraNote: string | null;
  /** The daemon's or the store's words, shown verbatim. */
  readonly error: string | null;
  /** Whether the camera should be running. */
  readonly camera: boolean;
}

/** The phases where a code is scanned or typed. */
const SCANNING_PHASES: ReadonlySet<PairingPhase> = new Set(["pending", "approved"]);

const CODE_ENTRY: CodeEntry = {
  label: "Six-digit code",
  inputMode: "numeric",
  autoComplete: "one-time-code",
  maxLength: 6,
  button: { label: "Pair", enabled: true },
};

/**
 * The screen for one pairing model, connection state and camera state. It
 * shows on the pairing channel, and while a paired phone finishes connecting;
 * every other state is the page's own content. The camera runs only while a
 * code can be scanned; the view starts and stops it from `camera`.
 */
export function pairingScreen(
  pairing: PairingState,
  status: ConnectionStateName,
  camera: CameraState,
): Screen {
  const visible = status === "pairing" || (pairing.phase === "paired" && status !== "ready");
  const scanning = visible && SCANNING_PHASES.has(pairing.phase);
  return {
    visible,
    title: "Pair this phone",
    body: bodyFor(pairing.phase),
    button: buttonFor(pairing.phase),
    scanner: scanning,
    entry: scanning ? CODE_ENTRY : null,
    cameraNote: scanning ? cameraNoteFor(camera) : null,
    error: pairing.error,
    camera: scanning,
  };
}

/** Whether a scanned code may be sent: not the one the daemon just refused. */
export function shouldSendScan(code: string, lastRefused: string | null): boolean {
  return code !== lastRefused;
}

/** The camera as the view tracks it: its state, and the number of its newest start. */
export interface CameraTrack {
  readonly state: CameraState;
  readonly attempt: number;
}

/** What the view asks of the camera. */
export type CameraEvent =
  | { readonly type: "start" }
  | { readonly type: "opened"; readonly attempt: number }
  | { readonly type: "failed"; readonly attempt: number; readonly state: CameraState }
  | { readonly type: "stop" };

export const CAMERA_OFF: CameraTrack = { state: "off", attempt: 0 };

/**
 * One camera event applied to the track. A start's own answer counts only
 * while its attempt is the newest, so a start that opened or failed late never
 * overwrites a newer one. A stop ends a running camera, and leaves a refused
 * or missing one as it is, so its note stands and nothing asks for it again.
 */
export function cameraStep(track: CameraTrack, event: CameraEvent): CameraTrack {
  switch (event.type) {
    case "start":
      return { state: "starting", attempt: track.attempt + 1 };
    case "opened":
      return track.attempt === event.attempt ? { ...track, state: "on" } : track;
    case "failed":
      return track.attempt === event.attempt ? { ...track, state: event.state } : track;
    case "stop":
      return track.state === "starting" || track.state === "on"
        ? { state: "off", attempt: track.attempt + 1 }
        : track;
  }
}

function bodyFor(phase: PairingPhase): string {
  switch (phase) {
    case "idle":
    case "denied":
      return "Ask your computer to let this phone in.";
    case "asking":
      return "Asking…";
    case "pending":
      return "Approve this phone on your computer, then scan the code it shows.";
    case "approved":
      return "Scan the code your computer shows, or type it.";
    case "paired":
      return "Paired. Connecting…";
  }
}

function buttonFor(phase: PairingPhase): ScreenButton | null {
  switch (phase) {
    case "idle":
    case "denied":
      return { label: "Ask to pair", enabled: true };
    case "asking":
      return { label: "Ask to pair", enabled: false };
    case "pending":
    case "approved":
    case "paired":
      return null;
  }
}

function cameraNoteFor(camera: CameraState): string | null {
  switch (camera) {
    case "denied":
      return "The camera is off for this app. Type the code instead.";
    case "unavailable":
      return "No camera found. Type the code instead.";
    case "off":
    case "starting":
    case "on":
      return null;
  }
}
