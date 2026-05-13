/**
 * Left-rail activity bar: picks which sidebar panel renders to its right.
 * Mirrors the VSCode pattern of a slim icon column owning the global mode.
 *
 * Active section persists to localStorage. The bar deliberately keeps no
 * other state — it's a controlled component driven by App.tsx so other
 * parts of the app (e.g. clicking "Open in source control" from a future
 * context menu) can switch sections without going through the bar.
 */
import Icon, { type IconName } from "./Icon";

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
  /// Total uncommitted file count across all registered repos. Renders as
  /// a VSCode-style numeric badge on the source-control button when > 0.
  /// 99+ collapses to "99+" so the badge stays a fixed pill size.
  sourceControlBadge: number;
}

export default function ActivityBar({
  active,
  onSelect,
  sourceControlBadge,
}: Props) {
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
        icon="sessions"
        badge={0}
      />
      <ActivityButton
        section="source-control"
        active={active}
        onSelect={onSelect}
        label="Source control"
        icon="sourceControl"
        badge={sourceControlBadge}
      />
    </nav>
  );
}

interface ActivityButtonProps {
  section: ActivitySection;
  active: ActivitySection;
  onSelect: (s: ActivitySection) => void;
  label: string;
  icon: IconName;
  badge: number;
}

function ActivityButton({
  section,
  active,
  onSelect,
  label,
  icon,
  badge,
}: ActivityButtonProps) {
  const isActive = section === active;
  const hasBadge = badge > 0;
  const badgeText = hasBadge ? (badge > 99 ? "99+" : String(badge)) : null;
  const fullLabel = hasBadge ? `${label} (${badge} uncommitted)` : label;
  return (
    <button
      type="button"
      role="tab"
      className={isActive ? "activity-btn active" : "activity-btn"}
      aria-selected={isActive}
      aria-label={fullLabel}
      title={fullLabel}
      data-testid={`activity-btn-${section}`}
      onClick={() => onSelect(section)}
    >
      <Icon name={icon} />
      {badgeText !== null && (
        <span
          className="activity-badge"
          aria-hidden="true"
          data-testid={`activity-badge-${section}`}
        >
          {badgeText}
        </span>
      )}
    </button>
  );
}
