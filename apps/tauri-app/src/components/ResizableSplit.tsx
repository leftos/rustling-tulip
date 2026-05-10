import { useCallback, useEffect, useRef, useState } from "react";

interface Props {
  /** Stable key used for localStorage persistence. */
  storageKey: string;
  /** Pixel size of the FIRST pane on first run. */
  defaultSize: number;
  /** Minimum size for either pane (in pixels). */
  minSize?: number;
  /** "horizontal" = vertical divider between two columns;
   *  "vertical"   = horizontal divider between two rows. */
  direction?: "horizontal" | "vertical";
  children: [React.ReactNode, React.ReactNode];
}

const PERSIST_NS = "rt:split:";

export default function ResizableSplit({
  storageKey,
  defaultSize,
  minSize = 120,
  direction = "horizontal",
  children,
}: Props) {
  const [size, setSize] = useState<number>(() => loadSize(storageKey, defaultSize));
  const containerRef = useRef<HTMLDivElement | null>(null);
  const draggingRef = useRef<boolean>(false);

  useEffect(() => {
    saveSize(storageKey, size);
  }, [storageKey, size]);

  const onPointerDown = useCallback(
    (ev: React.PointerEvent<HTMLDivElement>) => {
      ev.preventDefault();
      draggingRef.current = true;
      const target = ev.currentTarget;
      target.setPointerCapture(ev.pointerId);
    },
    [],
  );

  const onPointerMove = useCallback(
    (ev: React.PointerEvent<HTMLDivElement>) => {
      if (!draggingRef.current) return;
      const container = containerRef.current;
      if (!container) return;
      const rect = container.getBoundingClientRect();
      const raw =
        direction === "horizontal" ? ev.clientX - rect.left : ev.clientY - rect.top;
      const total = direction === "horizontal" ? rect.width : rect.height;
      const clamped = Math.max(minSize, Math.min(total - minSize, raw));
      setSize(clamped);
    },
    [direction, minSize],
  );

  const onPointerUp = useCallback(
    (ev: React.PointerEvent<HTMLDivElement>) => {
      draggingRef.current = false;
      ev.currentTarget.releasePointerCapture(ev.pointerId);
    },
    [],
  );

  const isHorizontal = direction === "horizontal";
  const gridStyle: React.CSSProperties = isHorizontal
    ? {
        display: "grid",
        gridTemplateColumns: `${size}px 4px 1fr`,
        height: "100%",
        minHeight: 0,
      }
    : {
        display: "grid",
        gridTemplateRows: `${size}px 4px 1fr`,
        width: "100%",
        minWidth: 0,
      };

  return (
    <div
      ref={containerRef}
      className={`split split-${direction}`}
      style={gridStyle}
    >
      <div className="split-pane">{children[0]}</div>
      <div
        className={`split-divider divider-${direction}`}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        role="separator"
        aria-orientation={isHorizontal ? "vertical" : "horizontal"}
      />
      <div className="split-pane">{children[1]}</div>
    </div>
  );
}

function loadSize(key: string, fallback: number): number {
  try {
    const v = localStorage.getItem(PERSIST_NS + key);
    if (!v) return fallback;
    const n = Number.parseFloat(v);
    return Number.isFinite(n) && n > 0 ? n : fallback;
  } catch {
    return fallback;
  }
}

function saveSize(key: string, size: number): void {
  try {
    localStorage.setItem(PERSIST_NS + key, String(size));
  } catch {
    /* ignore quota errors */
  }
}
