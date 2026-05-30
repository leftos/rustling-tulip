import { type ReactNode } from "react";

interface MenuSubmenuProps {
  label: string;
  children: ReactNode;
  disabled?: boolean;
  title?: string;
  dataTestId?: string;
  chip?: string;
  chipEmphasis?: boolean;
}

/// Hover-expandable submenu used inside the custom div-based context menus
/// (session menu, pane menu). The panel renders inline and is revealed via
/// CSS on hover/focus of the trigger; `disabled` hides the panel entirely.
export function MenuSubmenu({
  label,
  children,
  disabled = false,
  title,
  dataTestId,
  chip,
  chipEmphasis = false,
}: MenuSubmenuProps) {
  return (
    <li className="context-menu-submenu">
      <button
        type="button"
        className="context-menu-submenu-trigger"
        disabled={disabled}
        title={title}
        aria-haspopup="menu"
        data-testid={dataTestId}
        onClick={(e) => {
          e.preventDefault();
          e.currentTarget.focus();
        }}
      >
        <span className="context-menu-submenu-label">{label}</span>
        {chip && (
          <span
            className={
              chipEmphasis
                ? "context-menu-chip context-menu-chip-emph"
                : "context-menu-chip"
            }
          >
            {chip}
          </span>
        )}
        <span className="context-menu-submenu-arrow" aria-hidden="true">
          &gt;
        </span>
      </button>
      {!disabled && (
        <ul className="context-menu-submenu-panel" role="menu">
          {children}
        </ul>
      )}
    </li>
  );
}
