import type { ReactNode } from "react";

export type IconName =
  | "close"
  | "confirm"
  | "drag"
  | "extract"
  | "play"
  | "plus"
  | "refresh"
  | "repo"
  | "sessions"
  | "settings"
  | "sourceControl"
  | "splitDown"
  | "splitRight"
  | "workspace"
  | "stop";

interface Props {
  name: IconName;
  className?: string;
}

export default function Icon({ name, className }: Props) {
  return (
    <svg
      className={className ? `rt-icon ${className}` : "rt-icon"}
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      {iconPath(name)}
    </svg>
  );
}

function iconPath(name: IconName): ReactNode {
  switch (name) {
    case "close":
      return (
        <path d="M4 4l8 8M12 4l-8 8" />
      );
    case "confirm":
      return (
        <path d="M3.5 8.25l3 3L12.5 5" />
      );
    case "drag":
      return (
        <>
          <circle cx="6" cy="4" r="1" />
          <circle cx="10" cy="4" r="1" />
          <circle cx="6" cy="8" r="1" />
          <circle cx="10" cy="8" r="1" />
          <circle cx="6" cy="12" r="1" />
          <circle cx="10" cy="12" r="1" />
        </>
      );
    case "extract":
      return (
        <>
          <path d="M6 4h6v6" />
          <path d="M12 4l-8 8" />
          <path d="M4 5v7h7" />
        </>
      );
    case "play":
      return (
        <path d="M5 3.5v9l7-4.5z" />
      );
    case "plus":
      return (
        <>
          <path d="M8 3.5v9" />
          <path d="M3.5 8h9" />
        </>
      );
    case "refresh":
      return (
        <>
          <path d="M13 5a5 5 0 0 0-8.5-2.5L3 4" />
          <path d="M3 1.5V4h2.5" />
          <path d="M3 11a5 5 0 0 0 8.5 2.5L13 12" />
          <path d="M13 14.5V12h-2.5" />
        </>
      );
    case "repo":
      return (
        <>
          <path d="M4 3.5h6.5L12 5v7.5H4z" />
          <path d="M10.5 3.5V5H12" />
          <path d="M6 7h4" />
          <path d="M6 9.5h4" />
        </>
      );
    case "sessions":
      return (
        <>
          <path d="M3 4h10" />
          <path d="M3 8h10" />
          <path d="M3 12h10" />
        </>
      );
    case "settings":
      return (
        <>
          <circle cx="8" cy="8" r="2.25" />
          <path d="M8 2.75v1.35" />
          <path d="M8 11.9v1.35" />
          <path d="M3.25 8H4.6" />
          <path d="M11.4 8h1.35" />
          <path d="M4.65 4.65l.95.95" />
          <path d="M10.4 10.4l.95.95" />
          <path d="M11.35 4.65l-.95.95" />
          <path d="M5.6 10.4l-.95.95" />
        </>
      );
    case "sourceControl":
      return (
        <>
          <circle cx="5" cy="4" r="1.5" />
          <circle cx="5" cy="12" r="1.5" />
          <circle cx="11" cy="8" r="1.5" />
          <path d="M5 5.5v5" />
          <path d="M6.5 4.5c3 0 4.5 1.1 4.5 2" />
        </>
      );
    case "splitDown":
      return (
        <>
          <path d="M3 3h10v10H3z" />
          <path d="M3 8h10" />
          <path d="M8 5.25v5.5" />
          <path d="M5.75 8.75L8 11l2.25-2.25" />
        </>
      );
    case "splitRight":
      return (
        <>
          <path d="M3 3h10v10H3z" />
          <path d="M8 3v10" />
          <path d="M5.25 8h5.5" />
          <path d="M8.75 5.75L11 8l-2.25 2.25" />
        </>
      );
    case "stop":
      return (
        <path d="M4.5 4.5h7v7h-7z" />
      );
    case "workspace":
      return (
        <>
          <path d="M3.5 4.5h4v3h-4z" />
          <path d="M8.5 4.5h4v3h-4z" />
          <path d="M3.5 8.5h4v3h-4z" />
          <path d="M8.5 8.5h4v3h-4z" />
        </>
      );
  }
}
