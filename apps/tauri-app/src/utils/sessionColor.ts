import type { CSSProperties } from "react";

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
