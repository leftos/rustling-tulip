//! The preset wizard's prompt parsers, with the daemon's semantics: a
//! prompts text split into prompts, and a GitHub issue spec expanded into
//! issue numbers.

use std::collections::HashSet;

/// The most issues one spec may expand to: a fat-fingered range
/// (`123-12700`) must not spawn thousands of sessions.
pub const MAX_ISSUES_PER_SPEC: usize = 200;

/// How many issue numbers the summary under a spec lists before it counts
/// the rest.
const SUMMARY_HEAD: usize = 12;

/// Splits a prompts text into prompts: a leading `- ` or `-<tab>` (after
/// any indent) makes its line one prompt; other lines gather into a
/// paragraph that a blank line or a bullet ends. Empty prompts are dropped.
#[must_use]
pub fn parse_prompts(content: &str) -> Vec<String> {
    let mut prompts: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for raw in content.split('\n') {
        let line = raw.trim_end_matches('\r');
        let start = line.trim_start();
        if let Some(rest) = start
            .strip_prefix("- ")
            .or_else(|| start.strip_prefix("-\t"))
        {
            flush_block(&mut prompts, &mut current);
            let text = rest.trim();
            if !text.is_empty() {
                prompts.push(text.to_owned());
            }
        } else if line.trim().is_empty() {
            flush_block(&mut prompts, &mut current);
        } else {
            current.push(line);
        }
    }
    flush_block(&mut prompts, &mut current);
    prompts
}

/// Ends the paragraph gathered in `current`, keeping it when it holds text.
fn flush_block(prompts: &mut Vec<String>, current: &mut Vec<&str>) {
    if current.is_empty() {
        return;
    }
    let block = current.join("\n").trim().to_owned();
    current.clear();
    if !block.is_empty() {
        prompts.push(block);
    }
}

/// Expands a comma-separated GitHub issue spec (`123-127, 131`) into its
/// issue numbers, in order of first appearance and without duplicates.
/// Whitespace around tokens and the `-` is ignored, and empty tokens are
/// skipped.
///
/// # Errors
///
/// The daemon's message for a malformed token, a reversed range, a spec
/// with no numbers, or one that expands past [`MAX_ISSUES_PER_SPEC`].
pub fn parse_issue_spec(spec: &str) -> Result<Vec<u32>, String> {
    let mut issues: Vec<u32> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (start, end) = issue_range(token)?;
        for n in start..=end {
            if seen.insert(n) {
                issues.push(n);
            }
            if issues.len() > MAX_ISSUES_PER_SPEC {
                return Err(format!(
                    "issue spec expands to more than {MAX_ISSUES_PER_SPEC} issues; narrow the ranges"
                ));
            }
        }
    }
    if issues.is_empty() {
        return Err("no issue numbers found in the spec".to_owned());
    }
    Ok(issues)
}

/// The inclusive range one token names: `n-m`, or `n` alone.
fn issue_range(token: &str) -> Result<(u32, u32), String> {
    let Some((lo, hi)) = token.split_once('-') else {
        let n = parse_issue_number(token, token)?;
        return Ok((n, n));
    };
    let lo = parse_issue_number(lo.trim(), token)?;
    let hi = parse_issue_number(hi.trim(), token)?;
    if hi < lo {
        return Err(format!(
            "issue range '{token}' is reversed (end {hi} is before start {lo})"
        ));
    }
    Ok((lo, hi))
}

fn parse_issue_number(raw: &str, token: &str) -> Result<u32, String> {
    if raw.is_empty() {
        return Err(format!("issue token '{token}' is missing a number"));
    }
    let n: u32 = raw
        .parse()
        .map_err(|_| format!("issue token '{token}' is not a valid issue number"))?;
    if n == 0 {
        return Err(format!("issue number must be positive (got '{token}')"));
    }
    Ok(n)
}

/// The line under a valid issue spec: `3 issues → sessions: #7, #8, #9`,
/// listing the first twelve and counting the rest.
#[must_use]
pub fn issue_summary(issues: &[u32]) -> String {
    let (issue, session) = if issues.len() == 1 {
        ("issue", "session")
    } else {
        ("issues", "sessions")
    };
    let head: Vec<String> = issues
        .iter()
        .take(SUMMARY_HEAD)
        .map(|n| format!("#{n}"))
        .collect();
    let mut list = head.join(", ");
    if issues.len() > head.len() {
        list = format!("{list}, … (+{})", issues.len() - head.len());
    }
    format!("{} {issue} → {session}: {list}", issues.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn bullets_are_one_prompt_each() {
        assert_eq!(
            parse_prompts("- one\n- two\n- three\n"),
            strings(&["one", "two", "three"])
        );
    }

    #[test]
    fn paragraphs_are_one_prompt_each() {
        assert_eq!(
            parse_prompts("first prompt\nstill first\n\nsecond prompt\n"),
            strings(&["first prompt\nstill first", "second prompt"])
        );
    }

    #[test]
    fn bullets_and_paragraphs_mix() {
        assert_eq!(
            parse_prompts("- bullet one\n\nparagraph\nstill paragraph\n\n- bullet two\n"),
            strings(&["bullet one", "paragraph\nstill paragraph", "bullet two"])
        );
    }

    #[test]
    fn empty_and_blank_texts_have_no_prompts() {
        assert_eq!(parse_prompts(""), Vec::<String>::new());
        assert_eq!(parse_prompts("\n\n  \n"), Vec::<String>::new());
        assert_eq!(parse_prompts("   "), Vec::<String>::new());
    }

    #[test]
    fn a_bullet_ends_the_paragraph_before_it() {
        assert_eq!(
            parse_prompts("para\n- bullet\nnext para"),
            strings(&["para", "bullet", "next para"])
        );
    }

    #[test]
    fn indented_tab_and_crlf_bullets_are_bullets() {
        assert_eq!(
            parse_prompts("  - one\r\n-\ttwo\r\n\r\nthree\r\n"),
            strings(&["one", "two", "three"])
        );
    }

    #[test]
    fn an_empty_bullet_is_dropped_and_a_bare_dash_is_text() {
        assert_eq!(parse_prompts("- \n-   \n"), Vec::<String>::new());
        assert_eq!(parse_prompts("-\n"), strings(&["-"]));
    }

    #[test]
    fn a_paragraph_keeps_its_inner_indent_but_not_its_edges() {
        assert_eq!(
            parse_prompts("  first\n    second  \n"),
            strings(&["first\n    second"])
        );
    }

    #[test]
    fn ranges_and_singles_expand_in_order() {
        assert_eq!(
            parse_issue_spec("123-127, 131, 134-136"),
            Ok(vec![123, 124, 125, 126, 127, 131, 134, 135, 136])
        );
    }

    #[test]
    fn a_single_number_is_one_issue() {
        assert_eq!(parse_issue_spec("42"), Ok(vec![42]));
    }

    #[test]
    fn order_follows_first_appearance_and_overlaps_drop() {
        assert_eq!(
            parse_issue_spec("131, 123-127, 125-130"),
            Ok(vec![131, 123, 124, 125, 126, 127, 128, 129, 130])
        );
        assert_eq!(parse_issue_spec("5, 5, 5"), Ok(vec![5]));
    }

    #[test]
    fn whitespace_and_empty_tokens_are_ignored() {
        assert_eq!(parse_issue_spec("  7 - 9 ,, 12 ,"), Ok(vec![7, 8, 9, 12]));
    }

    #[test]
    fn malformed_tokens_are_refused_with_the_daemons_messages() {
        assert_eq!(
            parse_issue_spec("abc"),
            Err("issue token 'abc' is not a valid issue number".to_owned())
        );
        assert_eq!(
            parse_issue_spec("12-"),
            Err("issue token '12-' is missing a number".to_owned())
        );
        assert_eq!(
            parse_issue_spec("-12"),
            Err("issue token '-12' is missing a number".to_owned())
        );
        assert_eq!(
            parse_issue_spec("0"),
            Err("issue number must be positive (got '0')".to_owned())
        );
        assert_eq!(
            parse_issue_spec("1-2-3"),
            Err("issue token '1-2-3' is not a valid issue number".to_owned())
        );
    }

    #[test]
    fn a_reversed_range_is_refused() {
        assert_eq!(
            parse_issue_spec("12-9"),
            Err("issue range '12-9' is reversed (end 9 is before start 12)".to_owned())
        );
    }

    #[test]
    fn a_spec_without_numbers_is_refused() {
        let none = Err("no issue numbers found in the spec".to_owned());
        assert_eq!(parse_issue_spec(""), none);
        assert_eq!(parse_issue_spec("  ,  "), none);
    }

    #[test]
    fn the_cap_is_two_hundred_issues() {
        assert_eq!(
            parse_issue_spec("1-200").map(|issues| issues.len()),
            Ok(MAX_ISSUES_PER_SPEC)
        );
        let over = Err("issue spec expands to more than 200 issues; narrow the ranges".to_owned());
        assert_eq!(parse_issue_spec("1-201"), over);
        assert_eq!(parse_issue_spec("1-100000"), over);
        assert_eq!(
            parse_issue_spec("1-200, 1-200").map(|issues| issues.len()),
            Ok(MAX_ISSUES_PER_SPEC),
            "duplicates do not count against the cap"
        );
    }

    #[test]
    fn the_summary_names_one_issue_in_the_singular() {
        assert_eq!(issue_summary(&[42]), "1 issue → session: #42");
    }

    #[test]
    fn the_summary_lists_every_issue_up_to_twelve() {
        assert_eq!(issue_summary(&[7, 8, 9]), "3 issues → sessions: #7, #8, #9");
        let twelve: Vec<u32> = (1..=12).collect();
        assert_eq!(
            issue_summary(&twelve),
            "12 issues → sessions: #1, #2, #3, #4, #5, #6, #7, #8, #9, #10, #11, #12"
        );
    }

    #[test]
    fn the_summary_counts_the_issues_past_twelve() {
        let fifteen: Vec<u32> = (1..=15).collect();
        assert_eq!(
            issue_summary(&fifteen),
            "15 issues → sessions: #1, #2, #3, #4, #5, #6, #7, #8, #9, #10, #11, #12, … (+3)"
        );
    }
}
