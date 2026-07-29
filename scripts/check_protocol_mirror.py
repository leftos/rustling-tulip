#!/usr/bin/env python3
"""Check the hand-maintained TS mirror of the Rust wire protocol for drift.

`crates/protocol/src/lib.rs` is the single source of truth for the daemon/client
wire format, and the TypeScript types in `apps/tauri-app/src/` are maintained by
hand to match -- there is no codegen (see CLAUDE.md). A variant added on the Rust
side without its TS counterpart produces no compile error anywhere; it fails at
runtime, as a message that silently falls through the dispatcher's default arm.

This checks the one property that is both mechanical and worth catching: every
`ClientMessage` / `DaemonMessage` variant's snake_case serde tag appears
somewhere in the TS sources. It deliberately does NOT compare field-level shapes
-- that needs a real Rust parser, and the tag check catches the mistake people
actually make (adding a variant and forgetting the mirror).

Exit 0 when in sync, 1 with the missing tags listed otherwise.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

RUST_PROTOCOL = REPO_ROOT / "crates" / "protocol" / "src" / "lib.rs"
TS_SOURCES = [
    REPO_ROOT / "apps" / "tauri-app" / "src" / "types.ts",
    REPO_ROOT / "apps" / "tauri-app" / "src" / "api.ts",
    REPO_ROOT / "apps" / "tauri-app" / "src" / "App.tsx",
]

# Enums carrying `#[serde(tag = "type", rename_all = "snake_case")]`.
CHECKED_ENUMS = ["ClientMessage", "DaemonMessage"]


def to_snake_case(name: str) -> str:
    """`SessionUpdated` -> `session_updated`, matching serde's rename_all."""
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def enum_body(source: str, enum_name: str) -> str:
    """Return the brace-delimited body of `pub enum <enum_name>`."""
    match = re.search(r"pub enum " + re.escape(enum_name) + r"\s*\{", source)
    if not match:
        raise SystemExit(f"error: `pub enum {enum_name}` not found in {RUST_PROTOCOL}")
    depth = 1
    index = match.end()
    start = index
    while index < len(source) and depth > 0:
        char = source[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return source[start:index]
        index += 1
    raise SystemExit(f"error: unterminated body for enum {enum_name}")


def variant_names(body: str) -> list[str]:
    """Top-level variant identifiers, ignoring nested braces and parens."""
    names: list[str] = []
    depth = 0
    for line in body.split("\n"):
        stripped = line.strip()
        match = re.match(r"^([A-Z][A-Za-z0-9]*)\s*[\{\(,]", stripped)
        if match and depth == 0:
            names.append(match.group(1))
        depth += line.count("{") - line.count("}")
        depth += line.count("(") - line.count(")")
    return names


def main() -> int:
    if not RUST_PROTOCOL.is_file():
        print(f"error: missing {RUST_PROTOCOL}", file=sys.stderr)
        return 1
    rust_source = RUST_PROTOCOL.read_text(encoding="utf8")

    ts_blob = ""
    for path in TS_SOURCES:
        if not path.is_file():
            print(f"error: missing {path}", file=sys.stderr)
            return 1
        ts_blob += path.read_text(encoding="utf8")

    # Tags appear either as a literal type field or as a dispatcher case label.
    ts_tags = set(re.findall(r'type:\s*"([a-z0-9_]+)"', ts_blob))
    ts_tags |= set(re.findall(r'case\s+"([a-z0-9_]+)"', ts_blob))

    failed = False
    for enum_name in CHECKED_ENUMS:
        variants = variant_names(enum_body(rust_source, enum_name))
        if not variants:
            print(f"error: parsed zero variants from {enum_name}", file=sys.stderr)
            return 1
        missing = sorted(
            to_snake_case(v) for v in variants if to_snake_case(v) not in ts_tags
        )
        if missing:
            failed = True
            print(
                f"{enum_name}: {len(missing)} tag(s) have no TypeScript mirror:",
                file=sys.stderr,
            )
            for tag in missing:
                print(f"  - {tag}", file=sys.stderr)
        else:
            print(f"{enum_name}: {len(variants)} variants, all mirrored")

    if failed:
        print(
            "\nAdd the matching entry to apps/tauri-app/src/types.ts and handle it "
            "in App.tsx's message dispatcher.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
