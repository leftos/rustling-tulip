"""Collect "stop" tails from Claude Code transcripts for the local-summarizer spike.

Usage: uv run --no-project python -I extract.py <transcripts dir> <out dir> [--per-bucket N] [--seed S] [--extra FILE...]

A stop tail is the last assistant text of a turn whose final content block is text and which a user turn follows
(a typed prompt or a task notification, never a tool result). Top-level `*.jsonl` files only: `subagents` folders are
skipped. Candidates are bucketed by a rough heuristic (asks a question, mentions running work, reports work landed,
other) so the three
alert kinds are represented; the reference model's kind, not the bucket, is what the scoring uses.

Every sample is redacted (the daemon's secret shapes) and capped like the daemon's input (first 1,000 and last 5,000
characters with `[…]` between) before it is written as `<out dir>/<id>.md`. `--extra` copies further message files
(the earlier spike's three) in with the same redaction and cap. Samples hold transcript text: keep `<out dir>` outside
any repository.
"""

import argparse
import json
import random
import re
import sys
from pathlib import Path

SECRET_PATTERNS = [
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", re.DOTALL),
    re.compile(r"-----BEGIN [A-Z ]+-----.*?-----END [A-Z ]+-----", re.DOTALL),
    re.compile(r"\bsk-[A-Za-z0-9_\-]{16,}"),
    re.compile(r"\bghp_[A-Za-z0-9]{20,}"),
    re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}"),
    re.compile(r"\bxox[bp]-[A-Za-z0-9\-]{10,}"),
    re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
]
HEAD_CHARS = 1000
TAIL_CHARS = 5000
MIN_CHARS = 80
RUNNING_HINTS = re.compile(
    r"\b(still running|in the background|running in the background|waiting (on|for)|is running|are running|"
    r"will report|when it (finishes|lands|reports)|dispatched|in flight|monitor)\b",
    re.IGNORECASE,
)
DONE_HINTS = re.compile(
    r"\b(committed|landed|pushed|merged|shipped|nothing (else )?(is )?(running|in flight|pending)|all tests pass(ed)?|"
    r"suite (is )?green|done\.|complete\.)",
    re.IGNORECASE,
)


def redact(text: str) -> str:
    """Replace every secret-shaped string with `[redacted]`."""
    for pattern in SECRET_PATTERNS:
        text = pattern.sub("[redacted]", text)
    return text


def cap(text: str) -> str:
    """Keep the first 1,000 and last 5,000 characters, as the daemon's input shaping does."""
    if len(text) <= HEAD_CHARS + TAIL_CHARS:
        return text
    return text[:HEAD_CHARS] + "\n[…]\n" + text[-TAIL_CHARS:]


def is_user_prompt(entry: dict) -> bool:
    """True for a user turn that a person or a task notification wrote, not a tool result or meta line."""
    if entry.get("type") != "user" or entry.get("isMeta") or entry.get("isSidechain"):
        return False
    content = (entry.get("message") or {}).get("content")
    if isinstance(content, str):
        return not content.startswith(("<command-", "<local-command"))
    if isinstance(content, list):
        return any(block.get("type") == "text" for block in content) and not any(block.get("type") == "tool_result" for block in content)
    return False


def stop_tails(path: Path) -> list[str]:
    """Return every stop tail in one transcript, in order."""
    tails: list[str] = []
    last_text: str | None = None
    last_was_text = False
    with path.open(encoding="utf-8") as lines:
        for line in lines:
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            if entry.get("isSidechain"):
                continue
            if entry.get("type") == "assistant":
                for block in (entry.get("message") or {}).get("content") or []:
                    if block.get("type") == "text" and block.get("text", "").strip():
                        last_text, last_was_text = block["text"].strip(), True
                    elif block.get("type") == "tool_use":
                        last_was_text = False
            elif is_user_prompt(entry):
                if last_text and last_was_text and len(last_text) >= MIN_CHARS:
                    tails.append(last_text)
                last_text, last_was_text = None, False
    return tails


def bucket(text: str) -> str:
    """A rough guess at the alert kind, used only to balance the sample."""
    if text.rstrip().endswith("?") or "?" in text[-400:]:
        return "question"
    if RUNNING_HINTS.search(text):
        return "running"
    if DONE_HINTS.search(text[-1500:]):
        return "done"
    return "other"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("transcripts", type=Path)
    parser.add_argument("out", type=Path)
    parser.add_argument("--per-bucket", type=int, default=12)
    parser.add_argument("--seed", type=int, default=27)
    parser.add_argument("--extra", type=Path, nargs="*", default=[])
    args = parser.parse_args()

    buckets: dict[str, list[str]] = {"question": [], "running": [], "done": [], "other": []}
    seen: set[str] = set()
    for path in sorted(args.transcripts.glob("*.jsonl")):
        for tail in stop_tails(path):
            if tail in seen:
                continue
            seen.add(tail)
            buckets[bucket(tail)].append(tail)
    rng = random.Random(args.seed)
    args.out.mkdir(parents=True, exist_ok=True)
    written = 0
    for name, items in buckets.items():
        rng.shuffle(items)
        for index, text in enumerate(items[: args.per_bucket]):
            (args.out / f"{name}-{index:02d}.md").write_text(cap(redact(text)), encoding="utf-8", newline="\n")
            written += 1
        print(f"{name}: {len(items)} candidates, {min(len(items), args.per_bucket)} kept")
    for extra in args.extra:
        text = extra.read_text(encoding="utf-8").strip()
        (args.out / f"spike-{extra.stem}.md").write_text(cap(redact(text)), encoding="utf-8", newline="\n")
        written += 1
    print(f"wrote {written} samples to {args.out}")


if __name__ == "__main__":
    sys.exit(main())
