//! Typed partial-match ranking: whether a query matches a candidate's text,
//! and how well, so a list narrows to the matches with the best first.
//! Plain Rust; the spawn dialog's Target list ranks its rows with it.

/// How a query matched a text, best first: comparing two classes orders
/// them best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MatchClass {
    /// The text starts with the query.
    WholePrefix,
    /// A word of the text starts with the query: after a space, `-`, `_`,
    /// `/`, `\` or `.`.
    WordPrefix,
    /// The query appears whole somewhere in the text.
    Substring,
    /// Every character of the query appears in the text, in order.
    Subsequence,
}

/// One row to rank: the text it is found by, and a lower-ranked second text
/// (a target's host name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Candidate<'a> {
    pub primary: &'a str,
    pub secondary: Option<&'a str>,
}

/// How `query` matches `text`, case-insensitively; `None` when it does not.
/// The query is read trimmed, and an empty one matches every text as a
/// whole prefix.
#[must_use]
pub(crate) fn rank(query: &str, text: &str) -> Option<MatchClass> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(MatchClass::WholePrefix);
    }
    let text = text.to_lowercase();
    if text.starts_with(&query) {
        Some(MatchClass::WholePrefix)
    } else if word_starts(&text).any(|start| text[start..].starts_with(&query)) {
        Some(MatchClass::WordPrefix)
    } else if text.contains(&query) {
        Some(MatchClass::Substring)
    } else if is_subsequence(&query, &text) {
        Some(MatchClass::Subsequence)
    } else {
        None
    }
}

/// How well a candidate matched, best first: every match on the primary
/// text before any match on the secondary text alone, then by class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MatchKey {
    on_secondary: bool,
    class: MatchClass,
}

/// How `query` matches `candidate`: by its primary text when that matches
/// at all, else by its secondary text; `None` when neither does.
#[must_use]
pub(crate) fn match_key(query: &str, candidate: &Candidate<'_>) -> Option<MatchKey> {
    if let Some(class) = rank(query, candidate.primary) {
        return Some(MatchKey {
            on_secondary: false,
            class,
        });
    }
    let class = rank(query, candidate.secondary?)?;
    Some(MatchKey {
        on_secondary: true,
        class,
    })
}

/// The indices of the `candidates` that `query` matches, best first: any
/// match on the primary text before every match on the secondary text
/// alone, then by match class, then in the caller's order. An empty query
/// keeps every candidate in the caller's order.
#[cfg(test)]
#[must_use]
pub(crate) fn rank_all(query: &str, candidates: &[Candidate<'_>]) -> Vec<usize> {
    ranked(query, candidates)
        .into_iter()
        .map(|(_, index)| index)
        .collect()
}

/// [`rank_all`] with how well each candidate matched beside its index, so
/// a caller can compare matches across separately ranked lists.
#[must_use]
pub(crate) fn ranked(query: &str, candidates: &[Candidate<'_>]) -> Vec<(MatchKey, usize)> {
    let mut ranked: Vec<(MatchKey, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| match_key(query, candidate).map(|key| (key, index)))
        .collect();
    ranked.sort_unstable();
    ranked
}

/// Whether `c` ends a word, so a word starts after it.
fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '-' | '_' | '/' | '\\' | '.')
}

/// The byte offsets of `text` where a word starts after a separator.
fn word_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    text.char_indices()
        .filter(|(_, c)| is_separator(*c))
        .map(|(at, c)| at + c.len_utf8())
        .filter(move |start| *start < text.len())
}

/// Whether every character of `query` appears in `text`, in order.
fn is_subsequence(query: &str, text: &str) -> bool {
    let mut rest = text.chars();
    query.chars().all(|wanted| rest.any(|c| c == wanted))
}

#[cfg(test)]
mod tests {
    use super::{Candidate, MatchClass, rank, rank_all};

    fn named<'a>(names: &[&'a str]) -> Vec<Candidate<'a>> {
        names
            .iter()
            .map(|primary| Candidate {
                primary,
                secondary: None,
            })
            .collect()
    }

    #[test]
    fn empty_query_keeps_caller_order() {
        let candidates = named(&["zeta", "alpha", "mid"]);
        assert_eq!(rank_all("", &candidates), [0, 1, 2]);
        assert_eq!(rank_all("   ", &candidates), [0, 1, 2], "blank is empty");
        assert_eq!(rank("", "anything"), Some(MatchClass::WholePrefix));
    }

    #[test]
    fn whole_prefix_beats_word_prefix() {
        assert_eq!(rank("ya", "yaat"), Some(MatchClass::WholePrefix));
        assert_eq!(rank("ya", "my-yaat"), Some(MatchClass::WordPrefix));
        let candidates = named(&["my-yaat", "yaat"]);
        assert_eq!(rank_all("ya", &candidates), [1, 0]);
    }

    #[test]
    fn word_prefix_beats_substring() {
        for text in ["a b", "a-b", "a_b", "a/b", "a\\b", "a.b"] {
            assert_eq!(
                rank("b", text),
                Some(MatchClass::WordPrefix),
                "a word starts after the separator in {text:?}"
            );
        }
        assert_eq!(rank("to", "photo"), Some(MatchClass::Substring));
        let candidates = named(&["photo", "beta-tools"]);
        assert_eq!(rank_all("to", &candidates), [1, 0]);
    }

    #[test]
    fn substring_beats_subsequence() {
        assert_eq!(rank("ld", "build"), Some(MatchClass::Substring));
        assert_eq!(rank("ld", "lead"), Some(MatchClass::Subsequence));
        let candidates = named(&["lead", "build"]);
        assert_eq!(rank_all("ld", &candidates), [1, 0]);
    }

    #[test]
    fn subsequence_matches_in_order_only() {
        assert_eq!(rank("rtl", "rustling-tulip"), Some(MatchClass::Subsequence));
        assert_eq!(rank("ltr", "rustling"), None, "the letters out of order");
        assert_eq!(rank("aa", "ab"), None, "each query letter takes its own");
    }

    #[test]
    fn match_is_case_insensitive() {
        assert_eq!(rank("YA", "yaat"), Some(MatchClass::WholePrefix));
        assert_eq!(rank("ya", "YAAT"), Some(MatchClass::WholePrefix));
        assert_eq!(rank("ÉT", "été"), Some(MatchClass::WholePrefix));
    }

    #[test]
    fn primary_text_beats_secondary() {
        let candidates = [
            Candidate {
                primary: "r9",
                secondary: Some("box"),
            },
            Candidate {
                primary: "boxer",
                secondary: Some("box"),
            },
            Candidate {
                primary: "r1",
                secondary: Some("lab"),
            },
        ];
        assert_eq!(
            rank_all("box", &candidates),
            [1, 0],
            "the name's prefix before the host's; the other host's row drops"
        );
    }

    #[test]
    fn weak_primary_match_beats_strong_secondary_match() {
        let candidates = [
            Candidate {
                primary: "zeta",
                secondary: Some("box"),
            },
            Candidate {
                primary: "my-bot",
                secondary: Some("box"),
            },
        ];
        assert_eq!(
            rank_all("bo", &candidates),
            [1, 0],
            "a word prefix of the name beats a whole prefix of the host"
        );
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(rank("zzz", "yaat"), None);
        assert_eq!(rank_all("zzz", &named(&["yaat", "r0"])), [] as [usize; 0]);
    }
}
