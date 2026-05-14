import { useCallback, useEffect, useState } from "react";
import type { AppearanceOverrides } from "../types";
import {
  APPEARANCE_ACCENT_COLOR_PRESETS,
  APPEARANCE_BACKGROUND_COLOR_PRESETS,
  BUILT_IN_APPEARANCE,
  EMPTY_APPEARANCE,
  type ResolvedAppearance,
} from "../utils/appearance";
import { BUNDLED_FONTS, isBundledFont } from "../utils/bundledFonts";
import { MAX_FONT_SIZE, MIN_FONT_SIZE } from "../utils/fontSize";
import { rememberRecentCustomColor, useSettings } from "../utils/settings";
import Icon from "./Icon";

interface AppearanceFieldsProps {
  value: AppearanceOverrides;
  inherited: ResolvedAppearance;
  onChange: (next: AppearanceOverrides) => void;
  inheritLabel: string;
}

interface AppearanceModalProps extends AppearanceFieldsProps {
  title: string;
  onClose: () => void;
}

interface LocalFontData {
  family: string;
}

interface WindowWithLocalFonts {
  queryLocalFonts?: () => Promise<LocalFontData[]>;
}

export function AppearanceModal({
  title,
  value,
  inherited,
  inheritLabel,
  onChange,
  onClose,
}: AppearanceModalProps) {
  return (
    <div
      className="modal-backdrop modal-backdrop-preview"
      data-testid="appearance-modal"
    >
      <div
        className="modal modal-wide"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(e) => e.stopPropagation()}
      >
        <header className="modal-header">
          <h2>{title}</h2>
          <button
            type="button"
            className="modal-close"
            onClick={onClose}
            aria-label="Close dialog"
            title="Close"
          >
            <Icon name="close" />
          </button>
        </header>
        <div className="modal-body settings-body">
          <AppearanceFields
            value={value}
            inherited={inherited}
            inheritLabel={inheritLabel}
            onChange={onChange}
          />
        </div>
        <footer className="modal-footer">
          <button
            type="button"
            className="primary"
            onClick={onClose}
            data-testid="appearance-close"
          >
            Close
          </button>
        </footer>
      </div>
    </div>
  );
}

export function AppearanceFields({
  value,
  inherited,
  inheritLabel,
  onChange,
}: AppearanceFieldsProps) {
  const [settings] = useSettings();
  const update = <K extends keyof AppearanceOverrides>(
    key: K,
    next: AppearanceOverrides[K],
  ) => {
    onChange({ ...EMPTY_APPEARANCE, ...value, [key]: next });
  };
  const updateAccentFrame = (next: string | null) => {
    onChange({
      ...EMPTY_APPEARANCE,
      ...value,
      accent_color: next,
      terminal_frame_color: next,
    });
  };
  const accentFrameValue = value.accent_color ?? value.terminal_frame_color;
  const inheritedAccentFrame =
    inherited.values.accent_color ?? inherited.values.terminal_frame_color;
  const inheritedAccentFrameSource =
    inherited.values.accent_color !== null
      ? inherited.sources.accent_color
      : inherited.sources.terminal_frame_color;
  return (
    <section className="settings-section" data-testid="appearance-fields">
      <h3>Appearance</h3>
      <ColorRow
        label="Sidebar accent and pane frame"
        field="accent_frame_color"
        value={accentFrameValue}
        inherited={inheritedAccentFrame}
        source={inheritedAccentFrameSource}
        inheritLabel={inheritLabel}
        presets={APPEARANCE_ACCENT_COLOR_PRESETS}
        recentColors={settings.appearance_recent_colors}
        onCustomColor={rememberRecentCustomColor}
        onChange={updateAccentFrame}
      />
      <ColorRow
        label="Shell background"
        field="terminal_background_color"
        value={value.terminal_background_color}
        inherited={inherited.values.terminal_background_color}
        source={inherited.sources.terminal_background_color}
        inheritLabel={inheritLabel}
        presets={APPEARANCE_BACKGROUND_COLOR_PRESETS}
        recentColors={settings.appearance_recent_colors}
        onCustomColor={rememberRecentCustomColor}
        onChange={(next) => update("terminal_background_color", next)}
      />
      <FontFamilyRow
        value={value.terminal_font_family}
        inherited={inherited.values.terminal_font_family}
        source={inherited.sources.terminal_font_family}
        inheritLabel={inheritLabel}
        onChange={(next) => update("terminal_font_family", next)}
      />
      <div className="settings-row">
        <span>Font size</span>
        <div className="settings-row-control appearance-control-stack">
          <input
            type="range"
            min={MIN_FONT_SIZE}
            max={MAX_FONT_SIZE}
            step={1}
            value={value.terminal_font_size ?? inherited.values.terminal_font_size}
            onChange={(e) => {
              const next = Number.parseInt(e.target.value, 10);
              if (Number.isFinite(next)) update("terminal_font_size", next);
            }}
            data-testid="appearance-font-size"
            aria-label="Terminal font size"
          />
          <span className="muted small">
            {value.terminal_font_size ?? inherited.values.terminal_font_size}px
          </span>
          <ResetButton
            label={inheritLabel}
            disabled={value.terminal_font_size === null}
            onClick={() => update("terminal_font_size", null)}
          />
        </div>
      </div>
      <InheritedHint
        value={`${inherited.values.terminal_font_size}px`}
        source={inherited.sources.terminal_font_size}
      />
      <label className="settings-toggle">
        <input
          type="checkbox"
          checked={value.terminal_font_bold ?? inherited.values.terminal_font_bold}
          onChange={(e) => update("terminal_font_bold", e.target.checked)}
          data-testid="appearance-font-bold"
        />
        <span>Render terminal text in bold</span>
        <ResetButton
          label={inheritLabel}
          disabled={value.terminal_font_bold === null}
          onClick={() => update("terminal_font_bold", null)}
        />
      </label>
      <InheritedHint
        value={inherited.values.terminal_font_bold ? "bold" : "normal"}
        source={inherited.sources.terminal_font_bold}
      />
    </section>
  );
}

function ColorRow({
  label,
  field,
  value,
  inherited,
  source,
  inheritLabel,
  presets,
  recentColors,
  onCustomColor,
  onChange,
}: {
  label: string;
  field: string;
  value: string | null;
  inherited: string | null;
  source: string;
  inheritLabel: string;
  presets: readonly { name: string; color: string }[];
  recentColors: readonly string[];
  onCustomColor: (color: string) => void;
  onChange: (next: string | null) => void;
}) {
  const effective = value ?? inherited ?? BUILT_IN_APPEARANCE.terminal_background_color;
  const effectiveSource = value === null ? source : "current";
  const [draftColor, setDraftColor] = useState(effective);
  useEffect(() => {
    setDraftColor(effective);
  }, [effective]);
  const customChanged = draftColor !== effective;
  const applyCustomColor = () => {
    onChange(draftColor);
    onCustomColor(draftColor);
  };
  return (
    <>
      <div className="settings-row">
        <span>{label}</span>
        <div className="settings-row-control appearance-control-stack">
          <div
            className="appearance-swatch-list"
            role="group"
            aria-label={`${label} presets`}
          >
            {presets.map((preset) => (
              <button
                key={`${field}:${preset.color}`}
                type="button"
                className="appearance-swatch"
                style={{ background: preset.color }}
                title={`${preset.name} (${preset.color})`}
                aria-label={`Use ${preset.name}`}
                data-testid={`appearance-${field}-${preset.name.toLowerCase()}`}
                onClick={() => {
                  setDraftColor(preset.color);
                  onChange(preset.color);
                }}
              />
            ))}
          </div>
          {recentColors.length > 0 && (
            <div
              className="appearance-swatch-list appearance-recent-swatch-list"
              role="group"
              aria-label={`${label} recent custom colors`}
            >
              {recentColors.map((color, index) => (
                <button
                  key={`${field}:recent:${color}`}
                  type="button"
                  className="appearance-swatch"
                  style={{ background: color }}
                  title={`Recent custom color ${color}`}
                  aria-label={`Use recent custom color ${color}`}
                  data-color={color}
                  data-testid={`appearance-${field}-recent-${index}`}
                  onClick={() => {
                    setDraftColor(color);
                    onChange(color);
                  }}
                />
              ))}
            </div>
          )}
          <input
            type="color"
            value={draftColor}
            aria-label={label}
            data-testid={`appearance-${field}-custom`}
            onChange={(e) => setDraftColor(e.currentTarget.value)}
          />
          <button
            type="button"
            className="appearance-color-apply"
            disabled={!customChanged}
            data-testid={`appearance-${field}-custom-apply`}
            onClick={applyCustomColor}
          >
            Apply
          </button>
          <ResetButton
            label={inheritLabel}
            disabled={value === null}
            onClick={() => {
              setDraftColor(
                inherited ?? BUILT_IN_APPEARANCE.terminal_background_color,
              );
              onChange(null);
            }}
          />
        </div>
      </div>
      <InheritedHint value={effective} source={effectiveSource} />
    </>
  );
}

function FontFamilyRow({
  value,
  inherited,
  source,
  inheritLabel,
  onChange,
}: {
  value: string | null;
  inherited: string | null;
  source: string;
  inheritLabel: string;
  onChange: (next: string | null) => void;
}) {
  const [systemFonts, setSystemFonts] = useState<string[] | null>(null);
  const [systemFontsError, setSystemFontsError] = useState<string | null>(null);
  const canQueryLocalFonts =
    typeof window !== "undefined" &&
    typeof (window as WindowWithLocalFonts).queryLocalFonts === "function";
  const loadSystemFonts = useCallback(async () => {
    const w = window as WindowWithLocalFonts;
    if (!w.queryLocalFonts) return;
    try {
      const fonts = await w.queryLocalFonts();
      setSystemFonts(
        Array.from(new Set(fonts.map((font) => font.family))).sort((a, b) =>
          a.localeCompare(b),
        ),
      );
      setSystemFontsError(null);
    } catch (err) {
      setSystemFontsError(String(err));
    }
  }, []);
  const current = value ?? "";
  const known =
    current === "" ||
    isBundledFont(current) ||
    (systemFonts?.includes(current) ?? false);
  return (
    <>
      <div className="settings-row">
        <span>Font family</span>
        <div className="settings-row-control appearance-control-stack">
          <select
            value={current}
            onChange={(e) => onChange(e.target.value === "" ? null : e.target.value)}
            data-testid="appearance-font-family"
            aria-label="Terminal font family"
          >
            <option value="">{inheritLabel}</option>
            <optgroup label="Bundled">
              {BUNDLED_FONTS.map((font) => (
                <option key={font.family} value={font.family}>
                  {font.label}
                </option>
              ))}
            </optgroup>
            {systemFonts && systemFonts.length > 0 && (
              <optgroup label="System">
                {systemFonts.map((family) => (
                  <option key={`system:${family}`} value={family}>
                    {family}
                  </option>
                ))}
              </optgroup>
            )}
            {!known && current && (
              <optgroup label="Current">
                <option value={current}>{current} (saved)</option>
              </optgroup>
            )}
          </select>
          <button
            type="button"
            className="link"
            disabled={!canQueryLocalFonts}
            onClick={() => void loadSystemFonts()}
            data-testid="appearance-font-load-system"
          >
            {systemFonts === null
              ? "Load system fonts..."
              : `Refresh (${systemFonts.length} loaded)`}
          </button>
        </div>
      </div>
      <InheritedHint value={inherited ?? "default cascade"} source={source} />
      {systemFontsError && (
        <p className="settings-section-hint">
          Couldn't load system fonts: {systemFontsError}
        </p>
      )}
    </>
  );
}

function ResetButton({
  label,
  disabled,
  onClick,
}: {
  label: string;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className="link"
      disabled={disabled}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        onClick();
      }}
    >
      {label}
    </button>
  );
}

function InheritedHint({ value, source }: { value: string; source: string }) {
  return (
    <p className="settings-section-hint appearance-inherited">
      Resolved: {value} from {source}
    </p>
  );
}
