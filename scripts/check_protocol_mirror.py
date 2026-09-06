#!/usr/bin/env python3
"""Check the hand-maintained TS mirror of the Rust wire protocol for drift.

`crates/protocol/src/lib.rs` is the single source of truth for the daemon/client
wire format, and the TypeScript types in `apps/tauri-app/src/` are maintained by
hand to match -- there is no codegen (see CLAUDE.md). A variant added on the Rust
side without its TS counterpart produces no compile error anywhere; it fails at
runtime, as a message that silently falls through the dispatcher's default arm.

Two mechanical properties are checked, both chosen because they are the
mistakes people actually make:

1. Every `ClientMessage` / `DaemonMessage` variant's snake_case serde tag
   appears somewhere in the TS sources.
2. Every nested enum carrying `#[serde(other)] Unknown` has a matching
   `"unknown"` arm in its TS mirror. Those catch-alls are the forward-compat
   contract described in CLAUDE.md: the Rust side absorbs an unrecognized
   value in place so the containing message keeps decoding. A TS union that
   omits the arm claims a value it can actually receive is impossible, so a
   `switch` over it type-checks as exhaustive when it isn't.

Neither check compares field-level shapes -- that needs a real Rust parser.

Exit 0 when in sync, 1 with the problems listed otherwise.
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

# Rust enums whose `#[serde(other)]` catch-all is deliberately absent from the
# TS mirror, keyed to the reason. A catch-all only needs a TS arm when the enum
# can travel daemon -> client; for a client -> daemon-only type the Rust
# `Unknown` exists so an *older daemon* can decode a *newer client's* choice,
# and mirroring it would let the UI emit a value the daemon treats as a
# fallback. Add an entry only with that justification.
CATCH_ALL_EXEMPT = {
    "InitLayoutKind": (
        "client -> daemon only (ClientMessage::InitLayout); the Rust Unknown "
        "lets an older daemon decode a newer client's choice"
    ),
    "BranchCleanup": (
        "client -> daemon only (CleanupAction.branch on StopSession / "
        "DiscardSession); the Rust Unknown lets an older daemon decode a "
        "newer client's choice as Auto"
    ),
    "SuggestTarget": (
        "round-tripped echo (ClientMessage::SuggestBranchName -> "
        "DaemonMessage::BranchNameSuggestion): the daemon only ever sends back "
        "the variant the client sent it, so a client cannot receive Unknown"
    ),
}


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


def enums_with_catch_all(source: str) -> list[str]:
    """Names of `pub enum`s that carry a `#[serde(other)]` variant."""
    names: list[str] = []
    for match in re.finditer(r"pub enum ([A-Za-z0-9_]+)\s*\{", source):
        name = match.group(1)
        if "#[serde(other)]" in enum_body(source, name):
            names.append(name)
    return names


# String literals are listed before the comment alternatives so a `//` inside
# one (a URL, say) is consumed as part of the string and never treated as the
# start of a comment.
TS_STRING_OR_COMMENT = re.compile(
    r'"(?:\\.|[^"\\])*"'
    r"|'(?:\\.|[^'\\])*'"
    r"|`(?:\\.|[^`\\])*`"
    r"|//[^\n]*"
    r"|/\*.*?\*/",
    re.DOTALL,
)


def strip_ts_comments(source: str) -> str:
    """Blank out comments, preserving length and line structure.

    `ts_type_body` scans for a terminating semicolon, and prose comments
    routinely contain one -- without this, a declaration is truncated at the
    first semicolon in a comment and the check reports a phantom drift.
    """

    def blank(match: re.Match[str]) -> str:
        text = match.group(0)
        if not text.startswith(("//", "/*")):
            return text
        return "".join("\n" if char == "\n" else " " for char in text)

    return TS_STRING_OR_COMMENT.sub(blank, source)


def ts_type_body(ts_blob: str, name: str) -> str | None:
    """Right-hand side of `export type <name> = ...;`, or None if absent.

    Scans to the first semicolon at bracket depth zero so a multi-line union
    of object literals (`{ kind: "..." } | ...`) is captured whole. Expects a
    comment-stripped blob -- see `strip_ts_comments`.
    """
    match = re.search(r"export type " + re.escape(name) + r"\s*=", ts_blob)
    if not match:
        return None
    depth = 0
    for index in range(match.end(), len(ts_blob)):
        char = ts_blob[index]
        if char in "{([":
            depth += 1
        elif char in "})]":
            depth -= 1
        elif char == ";" and depth == 0:
            return ts_blob[match.end() : index]
    return None


def check_catch_alls(rust_source: str, ts_blob: str) -> list[str]:
    """Problems found; empty when every catch-all is mirrored or exempted."""
    problems: list[str] = []
    checked = 0
    ts_blob = strip_ts_comments(ts_blob)
    for name in enums_with_catch_all(rust_source):
        if name in CATCH_ALL_EXEMPT:
            continue
        body = ts_type_body(ts_blob, name)
        if body is None:
            problems.append(
                f"{name}: has #[serde(other)] but no `export type {name}` in the "
                f"TS sources"
            )
            continue
        checked += 1
        if '"unknown"' not in body:
            problems.append(
                f'{name}: has #[serde(other)] but its TS mirror has no "unknown" '
                f"arm -- a value it can receive would type-check as impossible"
            )
    if not problems:
        print(
            f"serde(other) catch-alls: {checked} mirrored, "
            f"{len(CATCH_ALL_EXEMPT)} exempt"
        )
    return problems


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

    catch_all_problems = check_catch_alls(rust_source, ts_blob)
    if catch_all_problems:
        failed = True
        print("\nserde(other) catch-alls missing from the TS mirror:", file=sys.stderr)
        for problem in catch_all_problems:
            print(f"  - {problem}", file=sys.stderr)
        print(
            "\nAdd the arm to the TS union. If the enum only travels client -> "
            "daemon, add it to CATCH_ALL_EXEMPT in this script with the reason "
            "instead.",
            file=sys.stderr,
        )

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
