"""Score bench.py result files against a reference run and print Markdown tables.

Usage: uv run --no-project python -I score.py <reference.jsonl> <label=candidate.jsonl[+more.jsonl]>... [--spike] [--disagreements]

The reference's earliest parsed round sets each sample's kind; its later rounds give the reference's run-to-run
agreement. A candidate's calls are scored only for samples the reference file holds, so a candidate run over a larger
folder (or several files joined with `+`) is cut down to the reference's sample set.
For each candidate (every round): calls, errors, strict JSON validity (the reply parses as one JSON object as is),
tolerant validity (after stripping a code fence and reading the first `{...}`, as the daemon's parser does), kind
agreement with the reference, field-cap violations (headline over 8 words, speech over 25, notification over 100
characters), identifier leaks in headline or speech, and warm and cold latency with llama-server's token rates.
`--spike` adds the earlier spike's three messages verbatim; `--disagreements` lists each kind disagreement.
"""

import argparse
import json
import re
import statistics
from pathlib import Path

KINDS = {"needs_answer", "working_update", "done_waiting"}
FIELDS = ("kind", "headline", "speech", "notification")
LEAK_PATTERNS = {
    "plan or issue number": re.compile(r"\b[A-Z]{1,5}-\d+\b|\bP\d+(\.\d+)+[a-z]?\b|\b[A-Z]{1,4}\d*\.\d+[a-z]?\b"),
    "code identifier": re.compile(r"\b[a-z][a-z0-9]*_[a-z0-9_]+\b|\b[a-z]+[A-Z][A-Za-z0-9]*\b|::|\w\(\)|`"),
    "path or hash": re.compile(r"\b[\w.-]+\.(rs|md|json|ps1|py|toml|ts)\b|[\\/]\w+[\\/]|\b[0-9a-f]{7,40}\b"),
}


def load(path: Path) -> tuple[list[dict], dict]:
    """Return a result file's call lines and its server figures (empty without --serve)."""
    calls, server = [], {}
    for line in path.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        if "server" in record:
            server.update(record["server"])
        elif "server_after" in record:
            server["after"] = record["server_after"]
        else:
            calls.append(record)
    return calls, server


def well_formed(value: object) -> dict | None:
    """Return the object when it has the four string fields and a known kind, else None."""
    if not isinstance(value, dict) or not all(isinstance(value.get(field), str) for field in FIELDS):
        return None
    return value if value["kind"] in KINDS else None


def parse_strict(text: str) -> dict | None:
    try:
        return well_formed(json.loads(text.strip()))
    except json.JSONDecodeError:
        return None


def parse_tolerant(text: str) -> dict | None:
    """Strip a code fence and read the first JSON object, as the design's parser does."""
    body = re.sub(r"^```(?:json)?\s*|\s*```$", "", text.strip())
    start = body.find("{")
    if start < 0:
        return None
    try:
        value, _ = json.JSONDecoder().raw_decode(body[start:])
    except json.JSONDecodeError:
        return None
    return well_formed(value)


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, round(fraction * (len(ordered) - 1)))]


def leaks(alert: dict) -> list[str]:
    text = f"{alert['headline']} {alert['speech']}"
    return [name for name, pattern in LEAK_PATTERNS.items() if pattern.search(text)]


def cap_violations(alert: dict) -> list[str]:
    found = []
    if len(alert["headline"].split()) > 8:
        found.append("headline")
    if len(alert["speech"].split()) > 25:
        found.append("speech")
    if len(alert["notification"]) > 100:
        found.append("notification")
    return found


def reference_kinds(calls: list[dict]) -> dict[str, str]:
    """Each sample's kind from the reference's earliest round whose reply parsed (a truncated reply has none)."""
    kinds: dict[str, str] = {}
    for call in sorted(calls, key=lambda c: c["round"]):
        alert = parse_tolerant(call.get("text", ""))
        if alert and call["sample"] not in kinds:
            kinds[call["sample"]] = alert["kind"]
    return kinds


def quality_row(label: str, calls: list[dict], ref: dict[str, str], disagreements: list[str]) -> str:
    """One quality-table row for a candidate's calls."""
    strict = tolerant = agree = compared = capped = leaked = 0
    for call in calls:
        text = call.get("text", "")
        strict += parse_strict(text) is not None
        alert = parse_tolerant(text)
        if not alert:
            continue
        tolerant += 1
        capped += bool(cap_violations(alert))
        leaked += bool(leaks(alert))
        if call["sample"] in ref:
            compared += 1
            if alert["kind"] == ref[call["sample"]]:
                agree += 1
            else:
                disagreements.append(f"{label} {call['sample']} r{call['round']}: ref {ref[call['sample']]}, got {alert['kind']}")
    n = len(calls)
    errors = sum("error" in call for call in calls)

    def pct(part: int, whole: int) -> str:
        return f"{100 * part / whole:.1f}% ({part}/{whole})" if whole else "-"

    return f"| {label} | {n} | {errors} | {pct(strict, n)} | {pct(tolerant, n)} | {pct(agree, compared)} | {capped} | {leaked} |"


def latency_row(label: str, calls: list[dict], server: dict) -> str:
    """One latency-table row: cold first call, warm p50 / p95, token rates, calls over 3 s and 8 s."""
    ok = [call for call in calls if "error" not in call]
    warm = [call["wall_s"] for call in ok if not call["cold"]]
    cold = next((call["wall_s"] for call in ok if call["cold"]), None)
    timings = [call["timings"] for call in ok if call.get("timings")]
    prompt_rate = statistics.median(t["prompt_per_second"] for t in timings) if timings else None
    gen_rate = statistics.median(t["predicted_per_second"] for t in timings) if timings else None
    prompt_tokens = statistics.median(t["prompt_n"] + t.get("cache_n", 0) for t in timings) if timings else None
    load = server.get("load_s", "-")
    over3 = sum(value > 3 for value in warm)
    over8 = sum(value > 8 for value in warm)
    rates = f"{prompt_rate:.0f} | {gen_rate:.1f} | {prompt_tokens:.0f}" if timings else "- | - | -"
    cold_text = f"{cold:.2f}" if cold is not None else "-"
    return (
        f"| {label} | {load} | {cold_text} | {percentile(warm, 0.5):.2f} | {percentile(warm, 0.95):.2f} | {max(warm):.2f} "
        f"| {over3} | {over8} | {rates} |"
    )


def memory_row(label: str, server: dict) -> str | None:
    if not server:
        return None
    after = server.get("after", {})
    return (
        f"| {label} | {server['vram_mib']} | {server['working_set_mib']} | {server['private_mib']} "
        f"| {after.get('working_set_mib', '-')} | {after.get('private_mib', '-')} |"
    )


def spike_rows(label: str, calls: list[dict]) -> list[str]:
    rows = []
    for call in calls:
        if call["sample"].startswith("spike-") and call["round"] == 0:
            alert = parse_tolerant(call.get("text", "")) or {}
            rows.append(
                f"| {call['sample'].removeprefix('spike-')} | {label} | `{alert.get('kind', 'invalid')}` | {alert.get('headline', '')} "
                f"| {alert.get('speech', '')} | {alert.get('notification', '')} | {call['wall_s']:.2f} s |"
            )
    return rows


def print_tables(args: argparse.Namespace, runs: list[tuple[str, list[dict], dict]], ref: dict[str, str]) -> list[str]:
    disagreements: list[str] = []
    print("| Run | calls | errors | strict JSON | tolerant JSON | kind agreement | cap violations | leaks |")
    print("|---|---|---|---|---|---|---|---|")
    for label, calls, _ in runs:
        print(quality_row(label, calls, ref, disagreements))
    print("\n| Run | load s | cold s | warm p50 s | warm p95 s | warm max s | warm > 3 s | warm > 8 s | prompt tok/s | gen tok/s | prompt tokens |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for label, calls, server in runs:
        print(latency_row(label, calls, server))
    memory = [row for label, _, server in runs if (row := memory_row(label, server))]
    if memory:
        print("\n| Run | VRAM MiB | working set MiB | private MiB | working set after MiB | private after MiB |")
        print("|---|---|---|---|---|---|")
        print("\n".join(memory))
    if args.spike:
        print("\n| Message | Run | kind | headline | speech | notification | wall |")
        print("|---|---|---|---|---|---|---|")
        for label, calls, _ in runs:
            print("\n".join(spike_rows(label, calls)))
    return disagreements


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("reference", type=Path)
    parser.add_argument("candidates", nargs="*")
    parser.add_argument("--spike", action="store_true")
    parser.add_argument("--disagreements", action="store_true")
    args = parser.parse_args()
    ref_calls, _ = load(args.reference)
    ref = reference_kinds(ref_calls)
    counts = {kind: sum(value == kind for value in ref.values()) for kind in sorted(KINDS)}
    print(f"Reference kinds over {len(ref)} samples: {counts}\n")
    runs = [("reference later rounds", [c for c in ref_calls if c["round"] > 0], {})]
    runs.insert(0, ("reference round 0", [c for c in ref_calls if c["round"] == 0], {}))
    reference_samples = {call["sample"] for call in ref_calls}
    for item in args.candidates:
        label, _, paths = item.partition("=")
        calls, server = [], {}
        for path in paths.split("+"):
            more, more_server = load(Path(path))
            calls += [call for call in more if call["sample"] in reference_samples]
            server = server or more_server
        runs.append((label, calls, server))
    disagreements = print_tables(args, runs, ref)
    if args.disagreements:
        print("\nDisagreements:\n" + "\n".join(disagreements))


if __name__ == "__main__":
    main()
