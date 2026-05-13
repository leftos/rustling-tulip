import type { CSSProperties } from "react";

export interface SessionColorPreset {
  name: string;
  color: string;
}

export const SESSION_COLOR_PRESETS = [
  { name: "Blue", color: "#2f81f7" },
  { name: "Green", color: "#3fb950" },
  { name: "Gold", color: "#f2cc60" },
  { name: "Red", color: "#ff7b72" },
  { name: "Purple", color: "#d2a8ff" },
  { name: "Cyan", color: "#56d4dd" },
  { name: "Orange", color: "#ffa657" },
  { name: "Sky", color: "#a5d6ff" },
  { name: "Mint", color: "#7ee787" },
  { name: "Copper", color: "#db6d28" },
  { name: "Pink", color: "#f778ba" },
  { name: "Slate", color: "#8b949e" },
] as const satisfies readonly SessionColorPreset[];

export const DEFAULT_SESSION_COLOR = SESSION_COLOR_PRESETS[0].color;

export function normalizeSessionColor(color: string | null | undefined): string | null {
  const trimmed = color?.trim();
  if (!trimmed || !/^#[0-9a-fA-F]{6}$/.test(trimmed)) return null;
  return trimmed.toLowerCase();
}

export function sessionAccentStyle(
  color: string | null | undefined,
): CSSProperties | undefined {
  const normalized = normalizeSessionColor(color);
  if (!normalized) return undefined;
  return { ["--session-accent" as string]: normalized } as CSSProperties;
}
