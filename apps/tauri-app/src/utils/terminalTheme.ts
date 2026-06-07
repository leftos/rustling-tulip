import type { ITheme } from "@xterm/xterm";

const BASE_THEME = {
  foreground: "#e5e6e8",
  cursor: "#5b9bff",
  cursorAccent: "#08090b",
  selectionBackground: "rgba(91, 155, 255, 0.30)",
  selectionForeground: "#f5f6f8",
  black: "#16181d",
  red: "#ef5c5c",
  green: "#3fb96a",
  yellow: "#e8a531",
  blue: "#5b9bff",
  magenta: "#b787f0",
  cyan: "#5dd5e3",
  white: "#e5e6e8",
  brightBlack: "#656872",
  brightRed: "#ff8585",
  brightGreen: "#62d18a",
  brightYellow: "#f5c267",
  brightBlue: "#7eb4ff",
  brightMagenta: "#d4abff",
  brightCyan: "#8feaf3",
  brightWhite: "#f5f6f8",
};

const ANSI_KEYS = [
  "black",
  "red",
  "green",
  "yellow",
  "blue",
  "magenta",
  "cyan",
  "white",
  "brightBlack",
  "brightRed",
  "brightGreen",
  "brightYellow",
  "brightBlue",
  "brightMagenta",
  "brightCyan",
  "brightWhite",
] as const;

interface Rgb {
  r: number;
  g: number;
  b: number;
}

export function buildTerminalTheme(background: string): ITheme {
  const bg = normalizeHex(background) ?? "#08090b";
  const foreground = ensureContrast(
    luminance(hexToRgb(bg)) < 0.5 ? BASE_THEME.foreground : "#1a1c22",
    bg,
    7,
  );
  // White caret, darkened toward visibility on light backgrounds. The
  // selection tint stays on the brand blue (BASE_THEME.cursor) so making
  // the cursor white doesn't recolor highlighted text.
  const cursor = ensureContrast("#ffffff", bg, 3);
  const selectionTint = ensureContrast(BASE_THEME.cursor, bg, 3);
  const theme: ITheme = {
    ...BASE_THEME,
    background: bg,
    foreground,
    cursor,
    cursorAccent: bg,
    selectionBackground: rgba(selectionTint, 0.3),
    selectionForeground: foreground,
  };
  for (const key of ANSI_KEYS) {
    theme[key] = ensureContrast(BASE_THEME[key], bg, key.startsWith("bright") ? 3 : 2.4);
  }
  return theme;
}

function normalizeHex(value: string): string | null {
  const trimmed = value.trim();
  if (!/^#[0-9a-fA-F]{6}$/.test(trimmed)) return null;
  return trimmed.toLowerCase();
}

function ensureContrast(color: string, background: string, minRatio: number): string {
  const bg = hexToRgb(background);
  const toward = luminance(bg) < 0.5 ? "#ffffff" : "#000000";
  let current = normalizeHex(color) ?? color;
  for (let i = 0; i <= 10; i += 1) {
    if (contrastRatio(hexToRgb(current), bg) >= minRatio) return current;
    current = mix(current, toward, 0.16);
  }
  return current;
}

function mix(color: string, toward: string, amount: number): string {
  const a = hexToRgb(color);
  const b = hexToRgb(toward);
  return rgbToHex({
    r: Math.round(a.r + (b.r - a.r) * amount),
    g: Math.round(a.g + (b.g - a.g) * amount),
    b: Math.round(a.b + (b.b - a.b) * amount),
  });
}

function rgba(color: string, alpha: number): string {
  const rgb = hexToRgb(color);
  return `rgba(${rgb.r}, ${rgb.g}, ${rgb.b}, ${alpha.toFixed(2)})`;
}

function contrastRatio(a: Rgb, b: Rgb): number {
  const light = Math.max(luminance(a), luminance(b));
  const dark = Math.min(luminance(a), luminance(b));
  return (light + 0.05) / (dark + 0.05);
}

function luminance(rgb: Rgb): number {
  const channels = [rgb.r, rgb.g, rgb.b].map((channel) => {
    const value = channel / 255;
    return value <= 0.03928
      ? value / 12.92
      : ((value + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * channels[0]! + 0.7152 * channels[1]! + 0.0722 * channels[2]!;
}

function hexToRgb(color: string): Rgb {
  const hex = normalizeHex(color) ?? "#000000";
  return {
    r: Number.parseInt(hex.slice(1, 3), 16),
    g: Number.parseInt(hex.slice(3, 5), 16),
    b: Number.parseInt(hex.slice(5, 7), 16),
  };
}

function rgbToHex(rgb: Rgb): string {
  const toHex = (value: number) => value.toString(16).padStart(2, "0");
  return `#${toHex(rgb.r)}${toHex(rgb.g)}${toHex(rgb.b)}`;
}
