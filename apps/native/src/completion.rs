//! The chat composer's completion popup, as plain Rust: which token the
//! caret sits in (a `/` at a line's start, an `@` at the text's start or
//! after whitespace), the popup's items, its wrapping selection, and the
//! text an accepted pick leaves. The text input finds the token with
//! [`token_at`] and applies a pick with [`apply_pick`]; the chat view keeps
//! one [`CompletionState`] per composer and only draws it.

use std::ops::Range;

/// The trigger that counts only at a line's start.
const LINE_START_TRIGGER: char = '/';

/// How many rows the popup shows at most.
pub const MAX_ROWS: usize = 8;

/// The token the caret sits in: its trigger, the text after the trigger up
/// to the caret, and its byte range in the text, trigger included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub trigger: char,
    pub query: String,
    pub range: Range<usize>,
}

/// One row of the popup: what it shows and the text a pick puts in place
/// of the token, trigger included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub insert: String,
}

impl CompletionItem {
    /// A row showing `label` that inserts `insert`.
    #[must_use]
    pub fn new(label: impl Into<String>, insert: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            insert: insert.into(),
        }
    }
}

/// The token the caret at byte `caret` of `text` sits in, for one of
/// `triggers`. A token runs from its trigger to the caret and holds no
/// whitespace, so a space ends it. A `/` counts only at a line's start;
/// any other trigger at the text's start or right after whitespace, never
/// inside a word (`a@b.c` holds none).
///
/// Args:
///     `text`: The input's text.
///     `caret`: The caret's byte offset in it.
///     `triggers`: The characters that start a token.
///
/// Returns:
///     The token, or None when the caret sits in none, or is not on a
///     character boundary of `text`.
#[must_use]
pub fn token_at(text: &str, caret: usize, triggers: &[char]) -> Option<Token> {
    let before = text.get(..caret)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8());
    let word = &before[start..];
    let trigger = word.chars().next().filter(|c| triggers.contains(c))?;
    let at_line_start = start == 0 || before[..start].ends_with('\n');
    if trigger == LINE_START_TRIGGER && !at_line_start {
        return None;
    }
    Some(Token {
        trigger,
        query: word[trigger.len_utf8()..].to_owned(),
        range: start..caret,
    })
}

/// `text` with `range` replaced by `insert` and one trailing space, and the
/// caret after that space.
///
/// Args:
///     `text`: The input's text.
///     `range`: The token's byte range, trigger included.
///     `insert`: The pick's insert text.
///
/// Returns:
///     The new text and caret, or None when `range` is not a range of
///     character boundaries inside `text`.
#[must_use]
pub fn apply_pick(text: &str, range: &Range<usize>, insert: &str) -> Option<(String, usize)> {
    let head = text.get(..range.start)?;
    let tail = text.get(range.end..)?;
    if range.start > range.end {
        return None;
    }
    let caret = head.len() + insert.len() + 1;
    Some((format!("{head}{insert} {tail}"), caret))
}

/// The rows a popup of `len` items, `selected` among them, shows: at most
/// [`MAX_ROWS`], the selected one always among them.
#[must_use]
pub fn visible_rows(len: usize, selected: usize) -> Range<usize> {
    let start = (selected + 1).saturating_sub(MAX_ROWS);
    start..len.min(start + MAX_ROWS)
}

/// One composer's popup: the items it offers, the token the caret sits in,
/// the selected row, and whether Esc closed it for this token.
#[derive(Debug, Default)]
pub struct CompletionState {
    items: Vec<CompletionItem>,
    token: Option<Token>,
    selected: usize,
    /// The start of the token Esc closed the popup on; the popup stays shut
    /// while the caret stays in a token starting there.
    dismissed: Option<usize>,
}

impl CompletionState {
    /// Offers `items`, the first selected.
    pub fn set_items(&mut self, items: Vec<CompletionItem>) {
        self.items = items;
        self.selected = 0;
    }

    /// The caret now sits in `token`: a token starting somewhere new selects
    /// the first row again and undoes an Esc.
    pub fn token_changed(&mut self, token: Token) {
        let same_start = self
            .token
            .as_ref()
            .is_some_and(|old| old.range.start == token.range.start);
        if !same_start {
            self.selected = 0;
        }
        if self.dismissed != Some(token.range.start) {
            self.dismissed = None;
        }
        self.token = Some(token);
    }

    /// The caret left the token.
    pub fn token_cleared(&mut self) {
        self.token = None;
        self.dismissed = None;
        self.selected = 0;
    }

    /// Esc: the popup shuts until the caret leaves this token.
    pub fn dismiss(&mut self) {
        self.dismissed = self.token.as_ref().map(|token| token.range.start);
    }

    /// Whether the popup shows: the caret sits in a token, Esc has not shut
    /// it, and it has items.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.token.is_some() && self.dismissed.is_none() && !self.items.is_empty()
    }

    /// The items offered.
    #[must_use]
    pub fn items(&self) -> &[CompletionItem] {
        &self.items
    }

    /// The selected row's index.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The token the caret sits in.
    #[must_use]
    pub fn token(&self) -> Option<&Token> {
        self.token.as_ref()
    }

    /// Down: the next row, from the last back to the first.
    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    /// Up: the row before, from the first round to the last.
    pub fn select_previous(&mut self) {
        if !self.items.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
        }
    }

    /// Selects row `index` when there is one.
    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }

    /// What accepting puts where: the token's range and the selected row's
    /// insert text; None while the popup is shut.
    #[must_use]
    pub fn pick(&self) -> Option<(Range<usize>, String)> {
        if !self.is_open() {
            return None;
        }
        let token = self.token.as_ref()?;
        let item = self.items.get(self.selected)?;
        Some((token.range.clone(), item.insert.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::{CompletionItem, CompletionState, Token, apply_pick, token_at, visible_rows};

    const TRIGGERS: &[char] = &['/', '@'];

    fn token(text: &str) -> Option<(char, String)> {
        token_at(text, text.len(), TRIGGERS).map(|token| (token.trigger, token.query))
    }

    #[test]
    fn slash_token_only_at_line_start() {
        assert_eq!(token("/co"), Some(('/', "co".to_owned())));
        assert_eq!(token("/"), Some(('/', String::new())));
        assert_eq!(token("a /co"), None, "a slash mid-line is no token");
        assert_eq!(token("x\n/co"), Some(('/', "co".to_owned())));
        assert_eq!(
            token_at("x\n/co", 5, TRIGGERS).map(|token| token.range),
            Some(2..5),
            "the token runs from its trigger to the caret"
        );
        assert_eq!(token("a/co"), None, "inside a word");
    }

    #[test]
    fn at_token_after_space_not_inside_an_email() {
        assert_eq!(token("@sr"), Some(('@', "sr".to_owned())));
        assert_eq!(token("see @sr"), Some(('@', "sr".to_owned())));
        assert_eq!(token("x\n@sr"), Some(('@', "sr".to_owned())));
        assert_eq!(token("me@sr"), None, "an email address is no token");
        assert_eq!(
            token_at("see @sr", 7, &['/']),
            None,
            "only the triggers asked for"
        );
    }

    #[test]
    fn a_space_ends_the_token() {
        assert_eq!(token("/co "), None);
        assert_eq!(token("@sr "), None);
        assert_eq!(
            token_at("/co x", 2, TRIGGERS).map(|token| token.query),
            Some("c".to_owned()),
            "the caret inside the token ends the query there"
        );
        assert_eq!(token_at("/co", 99, TRIGGERS), None, "a caret past the text");
    }

    #[test]
    fn accepting_replaces_only_the_token() {
        let text = "hi\n/co";
        let range = token_at(text, text.len(), TRIGGERS).map(|token| token.range);
        assert_eq!(range, Some(3..6));
        assert_eq!(
            apply_pick(text, &(3..6), "/compact"),
            Some(("hi\n/compact ".to_owned(), 12))
        );
        assert_eq!(
            apply_pick("see @sr and", &(4..7), "@src/main.rs"),
            Some(("see @src/main.rs  and".to_owned(), 17)),
            "the text after the token stays"
        );
        assert_eq!(
            apply_pick("ab", &(1..9), "x"),
            None,
            "a range past the text"
        );

        let mut state = CompletionState::default();
        state.set_items(vec![
            CompletionItem::new("clear", "/clear"),
            CompletionItem::new("compact", "/compact"),
        ]);
        assert_eq!(state.pick(), None, "no token, no pick");
        state.token_changed(Token {
            trigger: '/',
            query: "co".to_owned(),
            range: 3..6,
        });
        state.select_next();
        assert_eq!(state.pick(), Some((3..6, "/compact".to_owned())));
        state.dismiss();
        assert_eq!(state.pick(), None, "Esc shut it");
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut state = CompletionState::default();
        state.set_items(
            ["a", "b", "c"]
                .into_iter()
                .map(|label| CompletionItem::new(label, label))
                .collect(),
        );
        assert_eq!(state.selected(), 0);
        state.select_previous();
        assert_eq!(state.selected(), 2, "Up from the first goes to the last");
        state.select_next();
        assert_eq!(state.selected(), 0, "Down from the last goes to the first");
        state.select_next();
        assert_eq!(state.selected(), 1);
        assert_eq!(visible_rows(3, 1), 0..3);
        assert_eq!(visible_rows(20, 0), 0..8, "at most eight rows");
        assert_eq!(visible_rows(20, 12), 5..13, "the selected row in view");
    }
}
