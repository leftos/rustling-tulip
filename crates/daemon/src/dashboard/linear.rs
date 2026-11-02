//! A session's Linear issue, read from its branch name.
//!
//! A Linear branch names its issue often enough to fill the Dashboard's Issue
//! column without a Linear client: `rt-703`, `feat/rt-703-toast` and the like.

/// Branch words that look like a key but never name an issue, lower-cased: the
/// team list that would settle it needs a Linear client.
const NOT_KEYS: [&str; 20] = [
    "release",
    "hotfix",
    "fix",
    "feat",
    "feature",
    "bug",
    "version",
    "issue",
    "pr",
    "wip",
    "test",
    "tmp",
    "dependabot",
    "renovate",
    "utf",
    "sha",
    "md",
    "win",
    "py",
    "node",
];

/// The issue key `branch` names, if any: its last segment first, then the rest
/// from the first on.
pub(crate) fn issue_from_branch(branch: &str) -> Option<String> {
    let segments: Vec<&str> = branch.split('/').collect();
    let (last, rest) = segments.split_last()?;
    std::iter::once(last)
        .chain(rest.iter())
        .find_map(|segment| key_from_segment(segment))
}

/// The key `segment` begins with: a 2 to 10 letter-and-digit word starting with
/// a letter, `-`, its digits, and then end, `-`, `_`, `.` or a letter.
fn key_from_segment(segment: &str) -> Option<String> {
    let word_len = segment
        .bytes()
        .take_while(u8::is_ascii_alphanumeric)
        .count();
    if !(2..=10).contains(&word_len) {
        return None;
    }
    let (word, after_word) = segment.split_at(word_len);
    if !word.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) || !after_word.starts_with('-')
    {
        return None;
    }
    let tail = &after_word[1..];
    let digit_count = tail.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }
    let (digits, after_digits) = tail.split_at(digit_count);
    if !ends_key(after_digits) || NOT_KEYS.contains(&word.to_lowercase().as_str()) {
        return None;
    }
    Some(format!("{}-{digits}", word.to_ascii_uppercase()))
}

/// Whether what follows a key's digits ends the key. A `.` followed by a digit
/// starts a version number (`serde-1.0.200`), so the segment names no key.
fn ends_key(rest: &str) -> bool {
    let mut chars = rest.chars();
    match chars.next() {
        None | Some('-' | '_') => true,
        Some('.') => !chars.next().is_some_and(|c| c.is_ascii_digit()),
        Some(c) => c.is_ascii_alphabetic(),
    }
}

#[cfg(test)]
mod tests {
    use super::issue_from_branch;

    #[test]
    fn a_branch_names_its_issue() {
        for (branch, issue) in [
            ("rt-703", "RT-703"),
            ("feat/rt-703-toast", "RT-703"),
            ("RT-12_fix", "RT-12"),
            ("rt-327b", "RT-327"),
            ("rt-12.fix", "RT-12"),
        ] {
            assert_eq!(
                issue_from_branch(branch),
                Some(issue.to_owned()),
                "{branch}"
            );
        }
    }

    #[test]
    fn a_branch_without_a_key_has_none() {
        for branch in ["main", "feat/toasts", "v0.8.0"] {
            assert_eq!(issue_from_branch(branch), None, "{branch}");
        }
    }

    #[test]
    fn a_common_branch_word_is_not_a_key() {
        for branch in [
            "release-1",
            "hotfix-2",
            "fix-3-crash",
            "dependabot/cargo/serde-1.0.200",
            "utf-8-paths",
        ] {
            assert_eq!(issue_from_branch(branch), None, "{branch}");
        }
    }

    #[test]
    fn the_last_segment_wins_over_earlier_ones() {
        assert_eq!(issue_from_branch("rt-1/rt-2-x"), Some("RT-2".to_owned()));
    }

    #[test]
    fn a_digitless_prefix_is_ignored() {
        assert_eq!(issue_from_branch("rt-abc"), None);
    }
}
