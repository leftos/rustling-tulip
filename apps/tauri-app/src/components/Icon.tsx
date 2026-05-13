export type IconName =
  | "close"
  | "drag"
  | "extract"
  | "refresh"
  | "sessions"
  | "sourceControl"
  | "splitDown"
  | "splitRight";

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

function iconPath(name: IconName) {
  switch (name) {
    case "close":
      return (
        <path d="M4 4l8 8M12 4l-8 8" />
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
    case "refresh":
      return (
        <>
          <path d="M13 5a5 5 0 0 0-8.5-2.5L3 4" />
          <path d="M3 1.5V4h2.5" />
          <path d="M3 11a5 5 0 0 0 8.5 2.5L13 12" />
          <path d="M13 14.5V12h-2.5" />
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
  }
}
