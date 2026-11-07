import jsQR from "jsqr";

import { isPairingCode } from "./pairing";

/**
 * Reading the pairing code off the host's QR. This is the platform seam, like
 * `TokenStore`: the page runs [`createCodeScanner`] over
 * [`browserScannerEnv`], and a test hands `createCodeScanner` fakes for the
 * camera, the decoder and the frame source.
 */

/** How often a frame is decoded: about five times a second, on animation frames. */
export const SAMPLE_INTERVAL_MS = 200;

/** The code in `raw`, trimmed, when it is six digits; `null` for anything else. */
export function acceptCode(raw: string): string | null {
  const text = raw.trim();
  return isPairingCode(text) ? text : null;
}

/** The two decoders the page can run. */
export type DecoderKind = "barcode-detector" | "jsqr";

/**
 * Which decoder to use: the browser's own `BarcodeDetector` only when it
 * reports `qr_code`, else the bundled jsQR.
 */
export function chooseDecoder(platform: {
  readonly hasBarcodeDetector: boolean;
  readonly supportedFormats: readonly string[];
}): DecoderKind {
  return platform.hasBarcodeDetector && platform.supportedFormats.includes("qr_code")
    ? "barcode-detector"
    : "jsqr";
}

/** Reads the text in the current video frame, or `null` when it holds none. */
export interface Decoder {
  read(video: HTMLVideoElement): Promise<string | null>;
}

/** Where animation frames come from; a test's fake runs them on demand. */
export interface Frames {
  /** Run `tick` on the next frame; the returned function cancels it. */
  request(tick: () => void): () => void;
}

/** Everything the scanner takes from the platform. */
export interface ScannerEnv {
  getUserMedia(constraints: MediaStreamConstraints): Promise<MediaStream>;
  /** The decoder for this session, decided once from the platform's support. */
  decoder(): Promise<Decoder>;
  readonly frames: Frames;
  now(): number;
}

/**
 * Opens the camera and reads pairing codes until it is stopped. `onCode` is
 * handed each six-digit text; `true` takes it and ends the scan, `false`
 * leaves the scanner reading, for a code the caller will not send.
 */
export interface CodeScanner {
  start(video: HTMLVideoElement, onCode: (code: string) => boolean): Promise<void>;
  stop(): void;
}

/**
 * A scanner over one camera stream: it samples a frame about five times a
 * second on animation frames, hands each to the decoder, and calls `onCode`
 * with each text that is a pairing code, stopping when it answers `true`. A
 * camera that cannot open rejects `start`, for the caller to show, leaving no
 * track running; `stop` ends the loop, closes the camera and is safe to call
 * at any time, twice included.
 */
export function createCodeScanner(env: ScannerEnv): CodeScanner {
  let stream: MediaStream | undefined;
  let cancelFrame: (() => void) | undefined;
  let running = false;
  let generation = 0;

  function stop(): void {
    running = false;
    generation += 1;
    cancelFrame?.();
    cancelFrame = undefined;
    const current = stream;
    stream = undefined;
    for (const track of current?.getTracks() ?? []) {
      track.stop();
    }
  }

  async function start(video: HTMLVideoElement, onCode: (code: string) => void): Promise<void> {
    stop();
    running = true;
    const mine = generation;
    const opened = await env.getUserMedia({ video: { facingMode: "environment" }, audio: false });
    if (!running || mine !== generation) {
      // Stopped while the camera was opening: nothing will read this stream.
      for (const track of opened.getTracks()) {
        track.stop();
      }
      return;
    }
    stream = opened;
    video.srcObject = opened;
    let decoder: Decoder;
    try {
      decoder = await env.decoder();
    } catch (error: unknown) {
      // No decoder means no scan: give the camera back and let the caller say so.
      stop();
      throw error;
    }
    if (!running || mine !== generation) {
      return;
    }

    let lastSampleMs = Number.NEGATIVE_INFINITY;
    let found = false;
    let warned = false;

    function nextFrame(): void {
      cancelFrame = env.frames.request(() => {
        void sample();
      });
    }

    async function sample(): Promise<void> {
      cancelFrame = undefined;
      if (!running || mine !== generation || found) {
        return;
      }
      const millis = env.now();
      if (millis - lastSampleMs >= SAMPLE_INTERVAL_MS) {
        lastSampleMs = millis;
        let text: string | null = null;
        try {
          text = await decoder.read(video);
        } catch (error: unknown) {
          // The first frames can arrive before the video has one to read.
          if (!warned) {
            warned = true;
            console.warn("pwa: reading a camera frame failed", error);
          }
        }
        if (!running || mine !== generation) {
          return;
        }
        const code = text === null ? null : acceptCode(text);
        if (code !== null && onCode(code)) {
          found = true;
          return;
        }
      }
      nextFrame();
    }

    nextFrame();
  }

  return { start, stop };
}

/** The part of `BarcodeDetector` the scanner uses; the DOM lib does not ship it. */
export interface Detector {
  detect(source: HTMLVideoElement): Promise<readonly { readonly rawValue: string }[]>;
}

/** The `BarcodeDetector` constructor, reached only when the browser ships one. */
export interface DetectorClass {
  new (options?: { readonly formats?: readonly string[] }): Detector;
  getSupportedFormats(): Promise<readonly string[]>;
}

/**
 * The decoder for the platform: the browser's `BarcodeDetector` when it
 * reports `qr_code`, else `decodeFrame`, which is jsQR. A detector that cannot
 * say what it reads, or that refuses to be constructed, falls back instead of
 * failing the camera.
 */
export async function createDecoder(
  Detector: DetectorClass | undefined,
  decodeFrame: (video: HTMLVideoElement) => string | null,
): Promise<Decoder> {
  const detector = await builtInDetector(Detector);
  if (detector !== undefined) {
    return {
      read: async (video) => (await detector.detect(video))[0]?.rawValue ?? null,
    };
  }
  return {
    read: async (video) => decodeFrame(video),
  };
}

async function builtInDetector(Detector: DetectorClass | undefined): Promise<Detector | undefined> {
  if (Detector === undefined) {
    return undefined;
  }
  try {
    const supportedFormats = await Detector.getSupportedFormats();
    if (chooseDecoder({ hasBarcodeDetector: true, supportedFormats }) !== "barcode-detector") {
      return undefined;
    }
    return new Detector({ formats: ["qr_code"] });
  } catch (error: unknown) {
    console.warn("pwa: the built-in QR detector cannot start; using jsQR", error);
    return undefined;
  }
}

/**
 * The page's own camera, decoder and frames. jsQR runs on the main thread, so
 * the page ships one script and no worker file.
 */
export function browserScannerEnv(): ScannerEnv {
  return {
    getUserMedia: (constraints) => {
      const devices = navigator.mediaDevices;
      if (devices === undefined) {
        return Promise.reject(new DOMException("this browser has no camera API", "NotFoundError"));
      }
      return devices.getUserMedia(constraints);
    },
    decoder: () => createDecoder(barcodeDetectorClass(), readWithJsqr),
    frames: {
      request: (tick) => {
        const handle = window.requestAnimationFrame(() => {
          tick();
        });
        return () => {
          window.cancelAnimationFrame(handle);
        };
      },
    },
    now: () => window.performance.now(),
  };
}

/** `globalThis.BarcodeDetector` when the browser ships one. */
function barcodeDetectorClass(): DetectorClass | undefined {
  const candidate: unknown = (globalThis as { BarcodeDetector?: unknown }).BarcodeDetector;
  return typeof candidate === "function" ? (candidate as DetectorClass) : undefined;
}

let warnedContext = false;

/** jsQR over the video's current frame, drawn to a canvas the CSP allows. */
function readWithJsqr(video: HTMLVideoElement): string | null {
  const width = video.videoWidth;
  const height = video.videoHeight;
  if (width === 0 || height === 0) {
    return null;
  }
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  const context = canvas.getContext("2d", { willReadFrequently: true });
  if (context === null) {
    if (!warnedContext) {
      warnedContext = true;
      console.warn("pwa: no 2d canvas context to read camera frames with");
    }
    return null;
  }
  context.drawImage(video, 0, 0, width, height);
  const frame = context.getImageData(0, 0, width, height);
  return jsQR(frame.data, width, height)?.data ?? null;
}
