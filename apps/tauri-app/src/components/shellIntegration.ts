import type {
  IDecoration,
  IDisposable,
  IMarker,
  Terminal,
} from "@xterm/xterm";

/// Per-command record reconstructed from OSC 133 + OSC 633 marks emitted
/// by the daemon-injected shell-integration scripts. One record per
/// committed command; ephemeral until the matching OSC 133;D arrives, at
/// which point exit code + duration are filled in and the chip flips
/// from neutral to success/failure colour.
export type CommandStatus = "running" | "success" | "failure";

export interface CommandRecord {
  /// Monotonic id, used by callers to identify records across updates.
  id: number;
  /// Literal command text from OSC 633;E. Null when the shell didn't
  /// emit a 633;E (e.g. PowerShell without PSReadLine, or an Enter
  /// without a committed command). Callers should fall back to
  /// `readCommandFromBuffer` in that case.
  command: string | null;
  /// Exit code from OSC 133;D. Null while the command is still running.
  exitCode: number | null;
  /// Wall-clock duration between OSC 133;C and OSC 133;D, in ms.
  durationMs: number | null;
  status: CommandStatus;
  /// Best-effort scrape of the command text from the terminal buffer —
  /// used when `command` is null. Strips a heuristic prompt prefix.
  readCommandFromBuffer(): string;
  /// Output text between OSC 133;C and OSC 133;D, joined with `\n`.
  /// Trailing blank lines are trimmed. Returns "" before C fires.
  readOutput(): string;
}

export interface ChipClickEvent {
  record: CommandRecord;
  /// DOM element the chip dot is rendered into; used to anchor a
  /// floating menu.
  anchor: HTMLElement;
}

export interface ShellIntegrationOptions {
  onChipClick: (event: ChipClickEvent) => void;
}

interface InternalRecord extends CommandRecord {
  promptMarker: IMarker;
  outputMarker: IMarker | null;
  endLine: number | null;
  /// Wall-clock time the prompt was rendered (OSC 133;A fired). Used as
  /// a fallback start-time when OSC 133;C never arrives — better an
  /// over-estimate that includes user typing time than a null duration.
  promptShownAt: number;
  startedAt: number | null;
  endedAt: number | null;
  decoration: IDecoration | null;
  dotEl: HTMLElement | null;
}

/// Decode the VSCode OSC 633 escape encoding the daemon scripts apply
/// before sending the literal command text: `\\` for backslash, `\x3b`
/// for `;`, `\x0a` for newline. Anything else passes through unchanged.
function decodeOsc633Payload(s: string): string {
  let out = "";
  let i = 0;
  while (i < s.length) {
    const ch = s[i];
    if (ch === "\\" && i + 1 < s.length) {
      const next = s[i + 1];
      if (next === "\\") {
        out += "\\";
        i += 2;
        continue;
      }
      if (next === "x" && i + 3 < s.length) {
        const hex = s.slice(i + 2, i + 4);
        const code = Number.parseInt(hex, 16);
        if (!Number.isNaN(code)) {
          out += String.fromCharCode(code);
          i += 4;
          continue;
        }
      }
    }
    out += ch;
    i += 1;
  }
  return out;
}

/// Strip a heuristic prompt prefix from the first line of a command.
/// Looks for the last occurrence of `$ `, `> `, `# `, or `% ` and
/// returns everything after it. If none match the line is returned
/// unchanged.
function stripPromptPrefix(line: string): string {
  for (const sep of ["$ ", "> ", "# ", "% "]) {
    const idx = line.lastIndexOf(sep);
    if (idx >= 0) return line.slice(idx + sep.length);
  }
  return line;
}

export interface ShellIntegrationHandle extends IDisposable {
  /// Live snapshot of all known records. Mutated in place as marks
  /// arrive; do not retain across paint frames if you need a stable
  /// view.
  records: ReadonlyArray<CommandRecord>;
}

export function attachShellIntegration(
  term: Terminal,
  opts: ShellIntegrationOptions,
): ShellIntegrationHandle {
  const records: InternalRecord[] = [];
  let nextId = 1;
  let current: InternalRecord | null = null;

  const setDotStatus = (rec: InternalRecord) => {
    const el = rec.dotEl;
    if (!el) return;
    el.dataset["status"] = rec.status;
    let title: string;
    if (rec.status === "running") {
      title = "running…";
    } else {
      const exit = rec.exitCode ?? "?";
      const dur = formatDuration(rec.durationMs);
      title = `exit ${exit} · ${dur}`;
    }
    el.title = title;
  };

  const mountDecoration = (rec: InternalRecord) => {
    if (rec.decoration) return;
    const decoration = term.registerDecoration({
      marker: rec.promptMarker,
      x: 0,
      width: 1,
      layer: "top",
    });
    if (!decoration) return;
    rec.decoration = decoration;
    decoration.onRender((el) => {
      // Decoration onRender can fire multiple times for the same record
      // as xterm rebuilds layers (scroll, resize). Re-bind state each
      // time so the latest status/tooltip is in sync with the DOM.
      el.classList.add("rt-shell-chip");
      el.style.pointerEvents = "auto";
      let dot = el.querySelector<HTMLDivElement>(":scope > .rt-shell-chip-dot");
      if (!dot) {
        dot = document.createElement("div");
        dot.className = "rt-shell-chip-dot";
        el.appendChild(dot);
        el.addEventListener("click", (ev) => {
          ev.preventDefault();
          ev.stopPropagation();
          opts.onChipClick({ record: rec, anchor: el });
        });
      }
      rec.dotEl = el;
      setDotStatus(rec);
    });
  };

  const dropCurrent = () => {
    // Discard an in-flight record entirely — no chip is ever shown for
    // it. Triggered when a new OSC 133;A arrives before D fires (empty
    // Enter cycles, Ctrl+C cancels, or any other shell-induced re-prompt
    // that doesn't represent a real command). Better than synthesising
    // a phantom "exit 130" chip that the user has to mentally filter out.
    if (!current) return;
    current.promptMarker.dispose();
    current.outputMarker?.dispose();
    current = null;
  };

  const startRecord = () => {
    // OSC 133;A — a new prompt was just rendered. We do NOT mount a chip
    // here: the chip only appears once OSC 133;D arrives with a final
    // exit code, so the user never sees a transient grey "running" dot
    // sitting on the live prompt line.
    dropCurrent();
    const marker = term.registerMarker(0);
    if (!marker) {
      current = null;
      return;
    }
    const rec: InternalRecord = {
      id: nextId++,
      command: null,
      exitCode: null,
      durationMs: null,
      status: "running",
      promptMarker: marker,
      outputMarker: null,
      endLine: null,
      promptShownAt: performance.now(),
      startedAt: null,
      endedAt: null,
      decoration: null,
      dotEl: null,
      readCommandFromBuffer: () => readCommandFromBuffer(term, rec),
      readOutput: () => readOutput(term, rec),
    };
    current = rec;
  };

  const handleOsc133 = (payload: string): boolean => {
    // payload examples: "A", "B", "C", "D;0", "D;1"
    const semi = payload.indexOf(";");
    const code = semi >= 0 ? payload.slice(0, semi) : payload;
    const rest = semi >= 0 ? payload.slice(semi + 1) : "";
    switch (code) {
      case "A":
        startRecord();
        return true;
      case "B":
        // Prompt finished, before user input. No record state change —
        // we already started the record on A.
        return true;
      case "C": {
        // Command output starts. Capture the marker + timestamp so the
        // D handler can compute an accurate duration; no chip is
        // mounted yet — that happens on D.
        if (!current) return true;
        const marker = term.registerMarker(0);
        if (marker) current.outputMarker = marker;
        current.startedAt = performance.now();
        return true;
      }
      case "D": {
        if (!current) return true;
        const exit = Number.parseInt(rest, 10);
        current.exitCode = Number.isFinite(exit) ? exit : null;
        current.endLine = term.buffer.active.baseY + term.buffer.active.cursorY;
        current.endedAt = performance.now();
        // Prefer the C-mark timestamp (true command start) when we have
        // it; otherwise fall back to the prompt-render timestamp from A.
        // The A fallback over-counts by the user's typing time, but a
        // wall-clock estimate is more useful than an em-dash when the
        // shell didn't manage to emit C (PSReadLine unavailable, some
        // commands that bypass the AddToHistoryHandler, etc.).
        const startRef = current.startedAt ?? current.promptShownAt;
        current.durationMs = Math.max(0, current.endedAt - startRef);
        current.status = current.exitCode === 0 ? "success" : "failure";
        // Push to history and mount the chip now — the user only ever
        // sees chips for completed commands, never a grey "running" dot
        // on the live prompt line.
        records.push(current);
        mountDecoration(current);
        setDotStatus(current);
        // Detach `current` so the next A starts a clean record without
        // any in-flight reference to the just-completed one.
        current = null;
        return true;
      }
      default:
        return false;
    }
  };

  const handleOsc633 = (payload: string): boolean => {
    // payload examples: "E;ls -la", "P;Cwd=...", others
    const semi = payload.indexOf(";");
    if (semi < 0) return false;
    const code = payload.slice(0, semi);
    const rest = payload.slice(semi + 1);
    if (code === "E") {
      if (current) current.command = decodeOsc633Payload(rest);
      return true;
    }
    // Unhandled subcodes (P, C, D, …) — return false so xterm logs at
    // most a debug message; the daemon currently only emits E.
    return false;
  };

  // xterm.js parser hooks. Both handlers return synchronously so they
  // don't interleave with other addons' OSC handlers (e.g. clipboard's
  // OSC 52).
  const osc133 = term.parser.registerOscHandler(133, handleOsc133);
  const osc633 = term.parser.registerOscHandler(633, handleOsc633);

  return {
    records,
    dispose() {
      osc133.dispose();
      osc633.dispose();
      for (const rec of records) {
        rec.decoration?.dispose();
        rec.promptMarker.dispose();
        rec.outputMarker?.dispose();
      }
      records.length = 0;
      if (current) {
        current.promptMarker.dispose();
        current.outputMarker?.dispose();
        current = null;
      }
    },
  };
}

function readCommandFromBuffer(term: Terminal, rec: InternalRecord): string {
  const promptLine = rec.promptMarker.line;
  if (promptLine < 0) return "";
  const outputLine = rec.outputMarker?.line ?? promptLine;
  const buf = term.buffer.active;
  const lines: string[] = [];
  for (let y = promptLine; y <= outputLine; y++) {
    const line = buf.getLine(y);
    if (!line) continue;
    lines.push(line.translateToString(true));
  }
  if (lines.length === 0) return "";
  lines[0] = stripPromptPrefix(lines[0] ?? "");
  return lines.join("\n").trimEnd();
}

function readOutput(term: Terminal, rec: InternalRecord): string {
  const start = rec.outputMarker?.line;
  const end = rec.endLine ?? term.buffer.active.baseY + term.buffer.active.cursorY;
  if (start == null || start < 0) return "";
  const buf = term.buffer.active;
  const lines: string[] = [];
  // OSC 133;C is emitted just before the command's first output, so
  // the output proper starts on the line *after* the marker (the
  // marker line itself still contains the trailing prompt + the
  // committed command's text in some shells).
  const firstOutputLine = start + 1;
  for (let y = firstOutputLine; y <= end; y++) {
    const line = buf.getLine(y);
    if (!line) continue;
    lines.push(line.translateToString(true));
  }
  while (lines.length > 0 && lines[lines.length - 1]?.trim() === "") {
    lines.pop();
  }
  return lines.join("\n");
}

function formatDuration(ms: number | null): string {
  if (ms == null) return "—";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 2 : 1)}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s - m * 60);
  return `${m}m ${rem}s`;
}

export { decodeOsc633Payload as __test_decodeOsc633Payload };
export { stripPromptPrefix as __test_stripPromptPrefix };
