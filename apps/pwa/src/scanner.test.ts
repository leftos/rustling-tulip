import { expect, test, vi } from "vitest";

import {
  SAMPLE_INTERVAL_MS,
  acceptCode,
  chooseDecoder,
  createCodeScanner,
  createDecoder,
  type CodeScanner,
  type Decoder,
  type Frames,
  type ScannerEnv,
} from "./scanner";

const VIDEO = { srcObject: null } as unknown as HTMLVideoElement;

/** The text a frame-decoding fallback hands back. */
const JSQR_TEXT = "000000";

/** Animation frames a test runs by hand, one at a time. */
class ManualFrames implements Frames {
  private tick: (() => void) | undefined;

  request(tick: () => void): () => void {
    this.tick = tick;
    return () => {
      this.tick = undefined;
    };
  }

  /** Whether the scanner asked for another frame. */
  pending(): boolean {
    return this.tick !== undefined;
  }

  run(): void {
    const tick = this.tick;
    this.tick = undefined;
    tick?.();
  }
}

/** A stream track whose `stop` a test counts. */
interface FakeTrack {
  stop: ReturnType<typeof vi.fn>;
}

function fakeTrack(): FakeTrack {
  return { stop: vi.fn() };
}

/** A built-in detector that reads one code. */
class QrDetector {
  static getSupportedFormats(): Promise<readonly string[]> {
    return Promise.resolve(["qr_code"]);
  }

  detect(): Promise<readonly { readonly rawValue: string }[]> {
    return Promise.resolve([{ rawValue: "123456" }]);
  }
}

/** A built-in detector that cannot say which formats it reads. */
class NoFormatsDetector {
  static getSupportedFormats(): Promise<readonly string[]> {
    return Promise.reject(new Error("formats unavailable"));
  }

  detect(): Promise<readonly { readonly rawValue: string }[]> {
    return Promise.resolve([]);
  }
}

/** A built-in detector that refuses to be constructed. */
class UnconstructableDetector {
  static getSupportedFormats(): Promise<readonly string[]> {
    return Promise.resolve(["qr_code"]);
  }

  detect(): Promise<readonly { readonly rawValue: string }[]> {
    return Promise.resolve([]);
  }

  constructor() {
    throw new Error("cannot construct");
  }
}

interface Wiring {
  readonly scanner: CodeScanner;
  readonly frames: ManualFrames;
  readonly tracks: FakeTrack[];
  readonly video: HTMLVideoElement;
  readonly clock: { millis: number };
}

/** A scanner over a fake camera, a fake decoder and hand-run frames. */
function wiring(read: Decoder["read"], tracks: FakeTrack[]): Wiring {
  const frames = new ManualFrames();
  const clock = { millis: 0 };
  const video = { srcObject: null } as unknown as HTMLVideoElement;
  const stream = { getTracks: () => tracks } as unknown as MediaStream;
  const env: ScannerEnv = {
    getUserMedia: () => Promise.resolve(stream),
    decoder: () => Promise.resolve({ read }),
    frames,
    now: () => clock.millis,
  };
  return { scanner: createCodeScanner(env), frames, tracks, video, clock };
}

/** Let the scanner's per-frame work settle. */
function flush(): Promise<void> {
  return new Promise<void>((resolve) => {
    setTimeout(resolve, 0);
  });
}

/** Run the frames the scanner asked for, a sample interval apart. */
async function pump(wired: Wiring, count: number): Promise<void> {
  for (let index = 0; index < count && wired.frames.pending(); index += 1) {
    wired.clock.millis += SAMPLE_INTERVAL_MS;
    wired.frames.run();
    await flush();
  }
}

test("scanner_accepts_only_a_bare_six_digit_code", () => {
  const accepted: [string, string][] = [
    ["123456", "123456"],
    [" 123456\n", "123456"],
  ];
  for (const [raw, code] of accepted) {
    expect(acceptCode(raw)).toBe(code);
  }

  const refused = ["12345", "1234567", "12a456", "https://host.example/123456", ""];
  for (const raw of refused) {
    expect(acceptCode(raw)).toBeNull();
  }
});

test("scanner_chooses_the_built_in_detector_only_when_it_reports_qr_code", () => {
  expect(chooseDecoder({ hasBarcodeDetector: true, supportedFormats: ["qr_code"] })).toBe(
    "barcode-detector",
  );
  expect(chooseDecoder({ hasBarcodeDetector: true, supportedFormats: ["ean_13"] })).toBe("jsqr");
  expect(chooseDecoder({ hasBarcodeDetector: false, supportedFormats: [] })).toBe("jsqr");
  expect(chooseDecoder({ hasBarcodeDetector: false, supportedFormats: ["qr_code"] })).toBe("jsqr");
});

test("scanner_falls_back_to_jsqr_when_the_built_in_detector_cannot_start", async () => {
  const unlisted = await createDecoder(NoFormatsDetector, () => JSQR_TEXT);
  await expect(unlisted.read(VIDEO)).resolves.toBe(JSQR_TEXT);

  const unconstructable = await createDecoder(UnconstructableDetector, () => JSQR_TEXT);
  await expect(unconstructable.read(VIDEO)).resolves.toBe(JSQR_TEXT);

  const builtIn = await createDecoder(QrDetector, () => JSQR_TEXT);
  await expect(builtIn.read(VIDEO)).resolves.toBe("123456");
});

test("scanner_stop_stops_every_track_and_is_safe_to_call_twice", async () => {
  const tracks = [fakeTrack(), fakeTrack()];
  const wired = wiring(() => Promise.resolve(null), tracks);

  await wired.scanner.start(wired.video, () => true);
  expect(wired.video.srcObject).not.toBeNull();

  wired.scanner.stop();
  for (const track of tracks) {
    expect(track.stop).toHaveBeenCalledTimes(1);
  }

  wired.scanner.stop();
  for (const track of tracks) {
    expect(track.stop).toHaveBeenCalledTimes(1);
  }
});

test("scanner_stops_the_camera_when_the_decoder_cannot_start", async () => {
  const tracks = [fakeTrack(), fakeTrack()];
  const frames = new ManualFrames();
  const stream = { getTracks: () => tracks } as unknown as MediaStream;
  const scanner = createCodeScanner({
    getUserMedia: () => Promise.resolve(stream),
    decoder: () => Promise.reject(new Error("no decoder")),
    frames,
    now: () => 0,
  });

  await expect(scanner.start(VIDEO, () => true)).rejects.toThrow("no decoder");
  for (const track of tracks) {
    expect(track.stop).toHaveBeenCalledTimes(1);
  }
  expect(frames.pending()).toBe(false);
});

test("scanner_skips_frames_without_a_code_and_delivers_the_first_code_once", async () => {
  const read = vi.fn<Decoder["read"]>();
  read.mockResolvedValueOnce("hello").mockResolvedValueOnce("654321").mockResolvedValue("111111");
  const wired = wiring(read, [fakeTrack()]);
  const onCode = vi.fn<(code: string) => boolean>().mockReturnValue(true);

  await wired.scanner.start(wired.video, onCode);
  await pump(wired, 3);

  expect(onCode).toHaveBeenCalledTimes(1);
  expect(onCode).toHaveBeenCalledWith("654321");
  expect(read).toHaveBeenCalledTimes(2);
});

test("scanner_keeps_scanning_while_the_caller_refuses_the_code", async () => {
  const read = vi.fn<Decoder["read"]>();
  read.mockResolvedValueOnce("654321").mockResolvedValueOnce("654321").mockResolvedValue("111111");
  const wired = wiring(read, [fakeTrack()]);
  const onCode = vi.fn<(code: string) => boolean>();
  onCode.mockReturnValueOnce(false).mockReturnValueOnce(false).mockReturnValue(true);

  await wired.scanner.start(wired.video, onCode);
  await pump(wired, 4);

  expect(onCode.mock.calls).toEqual([["654321"], ["654321"], ["111111"]]);
  expect(read).toHaveBeenCalledTimes(3);
  expect(wired.frames.pending()).toBe(false);
});
