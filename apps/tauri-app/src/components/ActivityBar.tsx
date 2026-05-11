/**
 * Left-rail activity bar: picks which sidebar panel renders to its right.
 * Mirrors the VSCode pattern of a slim icon column owning the global mode.
 *
 * Active section persists to localStorage. The bar deliberately keeps no
 * other state — it's a controlled component driven by App.tsx so other
 * parts of the app (e.g. clicking "Open in source control" from a future
 * context menu) can switch sections without going through the bar.
 */

export type ActivitySection = "sessions" | "source-control";

export const ACTIVITY_STORAGE_KEY = "rt.activity";

export function readActivitySection(): ActivitySection {
  try {
    const v = localStorage.getItem(ACTIVITY_STORAGE_KEY);
    return v === "source-control" ? "source-control" : "sessions";
  } catch {
    return "sessions";
  }
}

export function writeActivitySection(section: ActivitySection): void {
  try {
    localStorage.setItem(ACTIVITY_STORAGE_KEY, section);
  } catch {
    /* localStorage unavailable — best-effort */
  }
}

interface Props {
  active: ActivitySection;
  onSelect: (s: ActivitySection) => void;
}

export default function ActivityBar({ active, onSelect }: Props) {
  return (
    <nav
      className="activity-bar"
      role="tablist"
      aria-label="Activity bar"
      aria-orientation="vertical"
      data-testid="activity-bar"
    >
      <ActivityButton
        section="sessions"
        active={active}
        onSelect={onSelect}
        label="Sessions"
        // List icon (three horizontal bars). Plain Unicode glyph so we don't
        // pull in an icon library for two icons.
        glyph="☰"
      />
      <ActivityButton
        section="source-control"
        active={active}
        onSelect={onSelect}
        label="Source control"
        // Branch glyph
        glyph="⎇"
      />
    </nav>
  );
}

interface ActivityButtonProps {
  section: ActivitySection;
  active: ActivitySection;
  onSelect: (s: ActivitySection) => void;
  label: string;
  glyph: string;
}

function ActivityButton({
  section,
  active,
  onSelect,
  label,
  glyph,
}: ActivityButtonProps) {
  const isActive = section === active;
  return (
    <button
      type="button"
      role="tab"
      className={isActive ? "activity-btn active" : "activity-btn"}
      aria-selected={isActive}
      aria-label={label}
      title={label}
      data-testid={`activity-btn-${section}`}
      onClick={() => onSelect(section)}
    >
      <span aria-hidden="true">{glyph}</span>
    </button>
  );
}
