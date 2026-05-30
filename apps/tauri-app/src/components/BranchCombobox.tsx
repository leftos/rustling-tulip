import { useEffect, useRef, useState } from "react";

interface Props {
  value: string;
  onChange: (value: string) => void;
  /// All branches to offer.
  branches: string[];
  /// The repo's currently checked-out branch, annotated in the list.
  currentBranch: string | null;
  placeholder?: string;
  inputRef?: React.RefObject<HTMLInputElement | null>;
  testId?: string;
  /// When true, a non-matching typed value offers a "create branch" row.
  allowCreate?: boolean;
}

// Sentinel marking the synthetic "create branch" row in keyboard navigation.
const CREATE_ROW = "_create";

/// A typeable branch picker that lists *all* branches on focus rather than
/// hiding them behind the prefilled value (the failure of a raw <datalist>,
/// which filters its options by the input text). Typing filters the list; a
/// value that matches no branch offers a "create" row so a new branch can be
/// started in place. Selecting an option or pressing Enter commits it.
export default function BranchCombobox({
  value,
  onChange,
  branches,
  currentBranch,
  placeholder,
  inputRef,
  testId,
  allowCreate = true,
}: Props) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const localRef = useRef<HTMLInputElement | null>(null);
  const ref = inputRef ?? localRef;

  const trimmed = value.trim();
  const exact = branches.includes(trimmed);
  // Show everything when the field is empty or already equals a branch (the
  // common prefilled case); otherwise filter by substring.
  const filtered =
    trimmed === "" || exact
      ? branches
      : branches.filter((b) => b.toLowerCase().includes(trimmed.toLowerCase()));
  const showCreate = allowCreate && trimmed !== "" && !exact;
  const rows = showCreate ? [...filtered, CREATE_ROW] : filtered;

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    window.addEventListener("mousedown", onDown);
    return () => window.removeEventListener("mousedown", onDown);
  }, [open]);

  const commit = (next: string) => {
    onChange(next);
    setOpen(false);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setOpen(true);
      setActive((a) => Math.min(a + 1, Math.max(rows.length - 1, 0)));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => Math.max(a - 1, 0));
    } else if (e.key === "Enter" && open && rows.length > 0) {
      e.preventDefault();
      const row = rows[Math.min(active, rows.length - 1)];
      commit(row === CREATE_ROW ? trimmed : (row ?? trimmed));
    } else if (e.key === "Escape") {
      setOpen(false);
    }
  };

  return (
    <div className="branch-combobox" ref={wrapRef}>
      <input
        ref={ref}
        type="text"
        value={value}
        placeholder={placeholder}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
          setActive(0);
        }}
        onClick={() => setOpen(true)}
        onKeyDown={onKeyDown}
        spellCheck={false}
        autoComplete="off"
        role="combobox"
        aria-expanded={open}
        data-testid={testId}
      />
      {open && rows.length > 0 && (
        <ul className="branch-combobox-list" role="listbox">
          {filtered.map((b, i) => (
            <li
              key={b}
              role="option"
              aria-selected={i === active}
              className={i === active ? "active" : ""}
              onMouseEnter={() => setActive(i)}
              onMouseDown={(e) => {
                e.preventDefault();
                commit(b);
              }}
            >
              <span className="branch-combobox-name">{b}</span>
              {b === currentBranch && (
                <span className="branch-combobox-current">current</span>
              )}
            </li>
          ))}
          {showCreate && (
            <li
              role="option"
              aria-selected={active === filtered.length}
              className={
                active === filtered.length
                  ? "branch-combobox-create active"
                  : "branch-combobox-create"
              }
              onMouseEnter={() => setActive(filtered.length)}
              onMouseDown={(e) => {
                e.preventDefault();
                commit(trimmed);
              }}
            >
              Create branch “{trimmed}”
            </li>
          )}
        </ul>
      )}
    </div>
  );
}
