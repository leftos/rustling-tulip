/**
 * Parse a comma-separated GitHub issue spec into an ordered, de-duplicated
 * list of issue numbers. Mirrors the Rust parser in
 * crates/daemon/src/presets.rs (`parse_issue_spec`) so the dialog preview
 * matches exactly what the daemon will expand.
 *
 * Each token is either a single number (`131`) or an inclusive range
 * (`123-127`). Whitespace around tokens and the `-` is ignored; empty tokens
 * (e.g. a trailing comma) are skipped. Order follows first appearance and
 * duplicates (including overlaps between ranges) are dropped.
 *
 * Returns the expanded issue numbers on success, or a human-readable error
 * on a malformed token, a reversed range, an empty result, or a spec that
 * expands past `MAX_ISSUES_PER_SPEC`.
 */

/** Must match `MAX_ISSUES_PER_SPEC` in crates/daemon/src/presets.rs. */
export const MAX_ISSUES_PER_SPEC = 200;

export type ParseIssueSpecResult =
  | { ok: true; issues: number[] }
  | { ok: false; error: string };

export function parseIssueSpec(spec: string): ParseIssueSpecResult {
  const issues: number[] = [];
  const seen = new Set<number>();
  for (const rawToken of spec.split(",")) {
    const token = rawToken.trim();
    if (token === "") continue;
    const dash = token.indexOf("-");
    let start: number;
    let end: number;
    if (dash >= 0) {
      const lo = parseIssueNumber(token.slice(0, dash).trim(), token);
      if (lo.error) return { ok: false, error: lo.error };
      const hi = parseIssueNumber(token.slice(dash + 1).trim(), token);
      if (hi.error) return { ok: false, error: hi.error };
      if (hi.value < lo.value) {
        return {
          ok: false,
          error: `issue range '${token}' is reversed (end ${hi.value} is before start ${lo.value})`,
        };
      }
      start = lo.value;
      end = hi.value;
    } else {
      const n = parseIssueNumber(token, token);
      if (n.error) return { ok: false, error: n.error };
      start = n.value;
      end = n.value;
    }
    for (let n = start; n <= end; n++) {
      if (!seen.has(n)) {
        seen.add(n);
        issues.push(n);
      }
      if (issues.length > MAX_ISSUES_PER_SPEC) {
        return {
          ok: false,
          error: `issue spec expands to more than ${MAX_ISSUES_PER_SPEC} issues; narrow the ranges`,
        };
      }
    }
  }
  if (issues.length === 0) {
    return { ok: false, error: "no issue numbers found in the spec" };
  }
  return { ok: true, issues };
}

type ParsedNumber = { value: number; error?: undefined } | { value: 0; error: string };

function parseIssueNumber(raw: string, token: string): ParsedNumber {
  if (raw === "") {
    return { value: 0, error: `issue token '${token}' is missing a number` };
  }
  if (!/^[0-9]+$/.test(raw)) {
    return {
      value: 0,
      error: `issue token '${token}' is not a valid issue number`,
    };
  }
  const value = Number.parseInt(raw, 10);
  if (value === 0) {
    return { value: 0, error: `issue number must be positive (got '${token}')` };
  }
  return { value };
}
