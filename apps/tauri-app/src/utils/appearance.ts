import type {
  AppearanceOverrides,
  RepoEntry,
  SessionSnapshot,
  WorkspaceEntry,
} from "../types";
import type { Settings } from "./settings";

export type AppearanceField = keyof AppearanceOverrides;
export type AppearanceSource = "session" | "container" | "app" | "built-in";

/// App-wide default accent color — the same subtle blue as the CSS `--accent`
/// token (`styles.css`). Every pane carries this as its band/focus color out of
/// the box until someone picks a different one. Also offered as the first swatch
/// in `APPEARANCE_ACCENT_COLOR_PRESETS` so users can return to it explicitly.
export const DEFAULT_ACCENT_COLOR = "#5b9bff";

export interface EffectiveAppearance {
  accent_color: string | null;
  terminal_background_color: string;
  terminal_frame_color: string;
  terminal_font_family: string | null;
  terminal_font_size: number;
  terminal_font_bold: boolean;
}

export interface ResolvedAppearance {
  values: EffectiveAppearance;
  sources: Record<AppearanceField, AppearanceSource>;
}

export const EMPTY_APPEARANCE: AppearanceOverrides = {
  accent_color: null,
  terminal_background_color: null,
  terminal_frame_color: null,
  terminal_font_family: null,
  terminal_font_size: null,
  terminal_font_bold: null,
};

export const BUILT_IN_APPEARANCE: EffectiveAppearance = {
  accent_color: DEFAULT_ACCENT_COLOR,
  terminal_background_color: "#08090b",
  terminal_frame_color: "#08090b",
  terminal_font_family: null,
  terminal_font_size: 13,
  terminal_font_bold: false,
};

export const APPEARANCE_BACKGROUND_COLOR_PRESETS = [
  { name: "Default", color: "#08090b" },
  { name: "Graphite", color: "#111318" },
  { name: "Ink", color: "#0b1020" },
  { name: "Evergreen", color: "#07150f" },
  { name: "Aubergine", color: "#1a1024" },
  { name: "Paper", color: "#f6f4ef" },
] as const;

export const APPEARANCE_ACCENT_COLOR_PRESETS = [
  { name: "Default", color: DEFAULT_ACCENT_COLOR },
  { name: "Sky", color: "#38bdf8" },
  { name: "Blue", color: "#3b82f6" },
  { name: "Violet", color: "#8b5cf6" },
  { name: "Emerald", color: "#22c55e" },
  { name: "Amber", color: "#f59e0b" },
  { name: "Rose", color: "#fb7185" },
] as const;

export function normalizeAppearanceColor(
  color: string | null | undefined,
): string | null {
  const trimmed = color?.trim();
  if (!trimmed || !/^#[0-9a-fA-F]{6}$/.test(trimmed)) return null;
  return trimmed.toLowerCase();
}

export function normalizeAppearanceOverrides(
  appearance: AppearanceOverrides | null | undefined,
): AppearanceOverrides {
  return {
    accent_color: normalizeAppearanceColor(appearance?.accent_color),
    terminal_background_color: normalizeAppearanceColor(
      appearance?.terminal_background_color,
    ),
    terminal_frame_color: normalizeAppearanceColor(
      appearance?.terminal_frame_color,
    ),
    terminal_font_family: normalizeNullableString(
      appearance?.terminal_font_family,
    ),
    terminal_font_size: normalizeFontSize(appearance?.terminal_font_size),
    terminal_font_bold:
      typeof appearance?.terminal_font_bold === "boolean"
        ? appearance.terminal_font_bold
        : null,
  };
}

export function resolveAppearance(
  settings: Settings,
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
  session: SessionSnapshot,
  options?: { ignoreSession?: boolean },
): ResolvedAppearance {
  const container = containerAppearanceForSession(repos, workspaces, session);
  const sessionAppearance = options?.ignoreSession
    ? EMPTY_APPEARANCE
    : session.appearance;
  return resolveAppearanceLayers(
    sessionAppearance,
    container,
    settings.appearance,
  );
}

export function resolveAppearanceLayers(
  session: AppearanceOverrides | null | undefined,
  container: AppearanceOverrides | null | undefined,
  app: AppearanceOverrides | null | undefined,
): ResolvedAppearance {
  const sessionLayer = normalizeAppearanceOverrides(session);
  const containerLayer = normalizeAppearanceOverrides(container);
  const appLayer = normalizeAppearanceOverrides(app);

  return {
    values: {
      accent_color: resolveNullableField(
        "accent_color",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.accent_color,
      ).value,
      terminal_background_color: resolveStringField(
        "terminal_background_color",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_background_color,
      ).value,
      terminal_frame_color: resolveStringField(
        "terminal_frame_color",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_frame_color,
      ).value,
      terminal_font_family: resolveNullableField(
        "terminal_font_family",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_font_family,
      ).value,
      terminal_font_size: resolveNumberField(
        "terminal_font_size",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_font_size,
      ).value,
      terminal_font_bold: resolveBooleanField(
        "terminal_font_bold",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_font_bold,
      ).value,
    },
    sources: {
      accent_color: resolveNullableField(
        "accent_color",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.accent_color,
      ).source,
      terminal_background_color: resolveStringField(
        "terminal_background_color",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_background_color,
      ).source,
      terminal_frame_color: resolveStringField(
        "terminal_frame_color",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_frame_color,
      ).source,
      terminal_font_family: resolveNullableField(
        "terminal_font_family",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_font_family,
      ).source,
      terminal_font_size: resolveNumberField(
        "terminal_font_size",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_font_size,
      ).source,
      terminal_font_bold: resolveBooleanField(
        "terminal_font_bold",
        sessionLayer,
        containerLayer,
        appLayer,
        BUILT_IN_APPEARANCE.terminal_font_bold,
      ).source,
    },
  };
}

export function containerAppearanceForSession(
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
  session: SessionSnapshot,
): AppearanceOverrides | null {
  if (session.workspace_id) {
    return (
      workspaces.find((workspace) => workspace.id === session.workspace_id)
        ?.appearance ?? null
    );
  }
  const repoId = session.members[0]?.repo_id;
  if (!repoId) return null;
  return repos.find((repo) => repo.id === repoId)?.appearance ?? null;
}

function normalizeNullableString(value: string | null | undefined): string | null {
  const trimmed = value?.trim();
  return trimmed && trimmed.length > 0 ? trimmed : null;
}

function normalizeFontSize(value: number | null | undefined): number | null {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  return Math.max(8, Math.min(32, Math.round(value)));
}

function resolveNullableField<K extends AppearanceField>(
  key: K,
  session: AppearanceOverrides,
  container: AppearanceOverrides,
  app: AppearanceOverrides,
  builtIn: AppearanceOverrides[K],
): { value: NonNullable<AppearanceOverrides[K]> | null; source: AppearanceSource } {
  return resolveField(key, session, container, app, builtIn) as {
    value: NonNullable<AppearanceOverrides[K]> | null;
    source: AppearanceSource;
  };
}

function resolveStringField<K extends AppearanceField>(
  key: K,
  session: AppearanceOverrides,
  container: AppearanceOverrides,
  app: AppearanceOverrides,
  builtIn: string,
): { value: string; source: AppearanceSource } {
  return resolveField(key, session, container, app, builtIn) as {
    value: string;
    source: AppearanceSource;
  };
}

function resolveNumberField<K extends AppearanceField>(
  key: K,
  session: AppearanceOverrides,
  container: AppearanceOverrides,
  app: AppearanceOverrides,
  builtIn: number,
): { value: number; source: AppearanceSource } {
  return resolveField(key, session, container, app, builtIn) as {
    value: number;
    source: AppearanceSource;
  };
}

function resolveBooleanField<K extends AppearanceField>(
  key: K,
  session: AppearanceOverrides,
  container: AppearanceOverrides,
  app: AppearanceOverrides,
  builtIn: boolean,
): { value: boolean; source: AppearanceSource } {
  return resolveField(key, session, container, app, builtIn) as {
    value: boolean;
    source: AppearanceSource;
  };
}

function resolveField<K extends AppearanceField>(
  key: K,
  session: AppearanceOverrides,
  container: AppearanceOverrides,
  app: AppearanceOverrides,
  builtIn: AppearanceOverrides[K] | string | number | boolean,
): { value: AppearanceOverrides[K] | string | number | boolean; source: AppearanceSource } {
  if (session[key] !== null) return { value: session[key], source: "session" };
  if (container[key] !== null) {
    return { value: container[key], source: "container" };
  }
  if (app[key] !== null) return { value: app[key], source: "app" };
  return { value: builtIn, source: "built-in" };
}
