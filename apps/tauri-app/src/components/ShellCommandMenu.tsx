import { useEffect, useMemo } from "react";
import { createPortal } from "react-dom";
import type { CommandRecord } from "./shellIntegration";
import { copyToClipboard } from "../utils/clipboard";
import { useClampedMenuPosition } from "../utils/a11y";

interface Props {
  record: CommandRecord;
  anchor: HTMLElement;
  onClose: () => void;
  /// Optional re-run handler. When provided, the "Re-run command" item
  /// is enabled; the host wires it to write the command back to the
  /// PTY input stream (without auto-submitting — the user presses
  /// Enter to confirm).
  onRerun: ((command: string) => void) | null;
}

/// Floating action menu anchored to a shell-chip dot. Renders into the
/// document body via portal so the menu can spill outside the terminal
/// container's clipping region. Dismisses on outside-click, Escape, or
/// scroll/resize (the anchor moves when xterm reflows, and tracking
/// every reflow would add noise; closing is cheaper).
export default function ShellCommandMenu({
  record,
  anchor,
  onClose,
  onRerun,
}: Props) {
  // Compute desired anchor position from the chip's bounding rect; the
  // useClampedMenuPosition hook then measures the menu after mount and
  // pulls it back into the viewport if the bottom-or-right corner would
  // spill. The menu is fixed-positioned, so the scroll handler below
  // closes it rather than chasing the anchor.
  const desired = useMemo(() => {
    const rect = anchor.getBoundingClientRect();
    return { x: rect.right + 4, y: rect.top };
  }, [anchor]);
  const { ref: menuRef, position } = useClampedMenuPosition(desired);

  useEffect(() => {
    const onDocClick = (ev: MouseEvent) => {
      const menu = menuRef.current;
      if (!menu) return;
      if (ev.target instanceof Node && menu.contains(ev.target)) return;
      onClose();
    };
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") onClose();
    };
    const onScroll = () => onClose();
    // Run on next tick so the click that opened the menu doesn't
    // immediately close it.
    const t = window.setTimeout(() => {
      window.addEventListener("mousedown", onDocClick, { capture: true });
    }, 0);
    window.addEventListener("keydown", onKey);
    window.addEventListener("resize", onScroll);
    window.addEventListener("scroll", onScroll, { capture: true });
    return () => {
      window.clearTimeout(t);
      window.removeEventListener("mousedown", onDocClick, { capture: true });
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("resize", onScroll);
      window.removeEventListener("scroll", onScroll, { capture: true });
    };
  }, [onClose, menuRef]);

  const resolveCommand = (): string => {
    return record.command ?? record.readCommandFromBuffer();
  };

  const handleCopyCommand = () => {
    const cmd = resolveCommand();
    void copyToClipboard(cmd, "shell-cmd").catch(() => {});
    onClose();
  };
  const handleCopyOutput = () => {
    void copyToClipboard(record.readOutput(), "shell-output").catch(() => {});
    onClose();
  };
  const handleCopyBoth = () => {
    const cmd = resolveCommand();
    const out = record.readOutput();
    const text = out.length > 0 ? `${cmd}\n${out}` : cmd;
    void copyToClipboard(text, "shell-cmd+out").catch(() => {});
    onClose();
  };
  const handleRerun = () => {
    if (!onRerun) return;
    onRerun(resolveCommand());
    onClose();
  };

  const exitText =
    record.status === "running"
      ? "running…"
      : `exit ${record.exitCode ?? "?"}`;
  const durText =
    record.durationMs == null ? "" : ` · ${formatDuration(record.durationMs)}`;
  const canRerun = onRerun != null && resolveCommand().length > 0;

  return createPortal(
    <div
      ref={menuRef}
      className="rt-shell-menu"
      style={{ position: "fixed", top: position.top, left: position.left }}
      role="menu"
    >
      <div className="rt-shell-menu-info">
        {exitText}
        {durText}
      </div>
      <button
        type="button"
        className="rt-shell-menu-item"
        onClick={handleCopyCommand}
        role="menuitem"
      >
        Copy command
      </button>
      <button
        type="button"
        className="rt-shell-menu-item"
        onClick={handleCopyOutput}
        role="menuitem"
      >
        Copy output only
      </button>
      <button
        type="button"
        className="rt-shell-menu-item"
        onClick={handleCopyBoth}
        role="menuitem"
      >
        Copy command and output
      </button>
      <button
        type="button"
        className="rt-shell-menu-item"
        onClick={handleRerun}
        disabled={!canRerun}
        role="menuitem"
      >
        Re-run command
      </button>
    </div>,
    document.body,
  );
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 2 : 1)}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s - m * 60);
  return `${m}m ${rem}s`;
}
