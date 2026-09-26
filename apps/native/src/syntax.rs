//! Syntax highlighting for the diff view: the daemon's language id to a
//! grammar, a text parsed top to bottom into per-line token classes, and each
//! class's colour from the terminal palette.
//!
//! The grammars are two-face's extra set, loaded once on first use; callers
//! reach it from the background executor, never the UI thread. Classes come
//! from each token's scope stack, not from a syntect theme.

use std::ops::Range;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::Rgb;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::theme;

/// A text with more lines than this is left uncoloured.
pub const MAX_LINES: usize = 20_000;
/// A text with a line longer than this, in bytes, is left uncoloured.
pub const MAX_LINE_BYTES: usize = 10_000;
/// How long highlighting one text may take before it gives up.
pub const TIME_LIMIT: Duration = Duration::from_secs(2);
/// The contrast every class colour keeps against the diff's background.
pub const MIN_CONTRAST: f64 = 4.5;

/// What a token is, as far as its colour goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenClass {
    Comment,
    String,
    Keyword,
    Constant,
    Type,
    Function,
}

impl TokenClass {
    pub const ALL: [Self; 6] = [
        Self::Comment,
        Self::String,
        Self::Keyword,
        Self::Constant,
        Self::Type,
        Self::Function,
    ];

    /// The ANSI palette entry the class is drawn in.
    const fn ansi_index(self) -> usize {
        match self {
            Self::Comment => 8,
            Self::String => 2,
            Self::Keyword => 5,
            Self::Constant => 3,
            Self::Type => 6,
            Self::Function => 4,
        }
    }

    /// The class's place in [`TokenClass::ALL`] and in [`class_colors`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// One line's classified tokens: byte ranges within the line, on character
/// boundaries, in order, none overlapping and no two neighbours of one class.
pub type LineSpans = Vec<(Range<usize>, TokenClass)>;

/// Every line's spans, the first line first; lines split as the diff model
/// splits them.
pub type Highlighted = Vec<LineSpans>;

/// How a language id finds its grammar.
#[derive(Debug, Clone, Copy)]
enum Find {
    Extension(&'static str),
    Name(&'static str),
}

/// The daemon's language ids that have a grammar. `plaintext` and any id not
/// here are not highlighted; so is `powershell`, whose grammar two-face
/// leaves out of its fancy-regex set.
const LANGUAGES: [(&str, Find); 24] = [
    ("typescript", Find::Extension("ts")),
    ("javascript", Find::Extension("js")),
    ("rust", Find::Extension("rs")),
    ("python", Find::Extension("py")),
    ("json", Find::Extension("json")),
    ("markdown", Find::Extension("md")),
    ("css", Find::Extension("css")),
    ("scss", Find::Extension("scss")),
    ("html", Find::Extension("html")),
    ("xml", Find::Extension("xml")),
    ("yaml", Find::Extension("yaml")),
    ("toml", Find::Extension("toml")),
    ("shell", Find::Extension("sh")),
    ("go", Find::Extension("go")),
    ("java", Find::Extension("java")),
    ("kotlin", Find::Extension("kt")),
    ("swift", Find::Extension("swift")),
    ("c", Find::Extension("c")),
    ("cpp", Find::Extension("cpp")),
    ("csharp", Find::Extension("cs")),
    ("ruby", Find::Extension("rb")),
    ("php", Find::Extension("php")),
    ("sql", Find::Extension("sql")),
    ("dockerfile", Find::Name("Dockerfile")),
];

/// Scope prefixes and the class they give, the first match winning: so
/// `storage.type` is a type while the rest of `storage` is a keyword. The
/// declaring words grammars scope as `storage.type.function` and the like
/// (`fn`, `def`, `class`, `struct`) are keywords, not types.
const RULES: [(&str, TokenClass); 19] = [
    ("comment", TokenClass::Comment),
    ("string", TokenClass::String),
    ("storage.type.function", TokenClass::Keyword),
    ("storage.type.class", TokenClass::Keyword),
    ("storage.type.struct", TokenClass::Keyword),
    ("storage.type.enum", TokenClass::Keyword),
    ("storage.type.trait", TokenClass::Keyword),
    ("storage.type.impl", TokenClass::Keyword),
    ("storage.type.interface", TokenClass::Keyword),
    ("storage.type", TokenClass::Type),
    ("keyword", TokenClass::Keyword),
    ("storage", TokenClass::Keyword),
    ("constant.numeric", TokenClass::Constant),
    ("constant.language", TokenClass::Constant),
    ("constant.character", TokenClass::Constant),
    ("entity.name.type", TokenClass::Type),
    ("support.type", TokenClass::Type),
    ("entity.name.function", TokenClass::Function),
    ("support.function", TokenClass::Function),
];

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static CLASSIFIER: OnceLock<Vec<(Scope, TokenClass)>> = OnceLock::new();

/// The grammars, loaded on the first call. Slow the first time: call it off
/// the UI thread.
pub fn syntax_set() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// Why [`highlight`] left a text uncoloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uncoloured {
    /// More than [`MAX_LINES`] lines.
    TooManyLines,
    /// A line over [`MAX_LINE_BYTES`].
    LineTooLong,
    /// The deadline passed before the last line.
    TimedOut,
    /// The cancel flag was set.
    Cancelled,
    /// The grammar failed to parse a line.
    ParseFailed,
}

/// Whether the daemon's language id `language` has a grammar, without
/// loading the grammars: `false` for `plaintext` and unknown ids.
#[must_use]
pub fn has_grammar(language: &str) -> bool {
    LANGUAGES.iter().any(|(id, _)| *id == language)
}

/// The grammar for the daemon's language id `language`; `None` for
/// `plaintext` and ids without one. Loads the grammars on first use.
#[must_use]
pub fn syntax_for(language: &str) -> Option<&'static SyntaxReference> {
    let (_, find) = LANGUAGES.iter().find(|(id, _)| *id == language)?;
    let set = syntax_set();
    match *find {
        Find::Extension(ext) => set.find_syntax_by_extension(ext),
        Find::Name(name) => set
            .find_syntax_by_extension(name)
            .or_else(|| set.find_syntax_by_name(name)),
    }
}

/// `text` parsed with `syntax` from `set`, each line's tokens classified.
/// Checks `cancel` and `deadline` before each line; an error leaves the
/// text uncoloured and says why.
pub fn highlight(
    text: &str,
    syntax: &SyntaxReference,
    set: &SyntaxSet,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Highlighted, Uncoloured> {
    let lines: Vec<&str> = LinesWithEndings::from(text).collect();
    if lines.len() > MAX_LINES {
        return Err(Uncoloured::TooManyLines);
    }
    if lines.iter().any(|line| content_len(line) > MAX_LINE_BYTES) {
        return Err(Uncoloured::LineTooLong);
    }
    let rules = classifier();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut highlighted = Vec::with_capacity(lines.len());
    for line in lines {
        if cancel.load(Ordering::Relaxed) {
            return Err(Uncoloured::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Uncoloured::TimedOut);
        }
        let ops = state
            .parse_line(line, set)
            .map_err(|_| Uncoloured::ParseFailed)?;
        let len = content_len(line);
        let mut spans = LineSpans::new();
        let mut at = 0;
        for (next, op) in &ops {
            push(&mut spans, at..(*next).min(len), classify(rules, &stack));
            stack.apply(op).map_err(|_| Uncoloured::ParseFailed)?;
            at = *next;
        }
        push(&mut spans, at..len, classify(rules, &stack));
        highlighted.push(spans);
    }
    Ok(highlighted)
}

/// Each class's colour, in [`TokenClass::ALL`] order: its entry in `palette`
/// (the 16 ANSI colours) moved away from `background` until it reaches
/// [`MIN_CONTRAST`].
#[must_use]
pub fn class_colors(palette: &[Rgb; 16], background: Rgb) -> [Rgb; 6] {
    TokenClass::ALL.map(|class| {
        let color = palette[class.ansi_index()];
        theme::ensure_contrast(color, background, MIN_CONTRAST)
    })
}

/// A line's length without its `\n` or a `\r` before it, as the diff model
/// cuts its lines.
fn content_len(line: &str) -> usize {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line).len()
}

fn classifier() -> &'static [(Scope, TokenClass)] {
    CLASSIFIER.get_or_init(|| {
        RULES
            .iter()
            .filter_map(|(prefix, class)| Some((Scope::new(prefix).ok()?, *class)))
            .collect()
    })
}

/// The class of the innermost scope on `stack` that has one.
fn classify(rules: &[(Scope, TokenClass)], stack: &ScopeStack) -> Option<TokenClass> {
    stack.as_slice().iter().rev().find_map(|scope| {
        rules
            .iter()
            .find(|(prefix, _)| prefix.is_prefix_of(*scope))
            .map(|(_, class)| *class)
    })
}

/// Adds `range` as `class` to `spans`, joined to the last span when that one
/// has the same class and ends where it starts. An empty range or no class
/// adds nothing.
fn push(spans: &mut LineSpans, range: Range<usize>, class: Option<TokenClass>) {
    let Some(class) = class else {
        return;
    };
    if range.is_empty() {
        return;
    }
    if let Some((last, last_class)) = spans.last_mut()
        && *last_class == class
        && last.end == range.start
    {
        last.end = range.end;
        return;
    }
    spans.push((range, class));
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    /// Room for a debug build to parse a text at a cutoff's limit.
    const SLOW: Duration = Duration::from_secs(120);

    const IDS: [&str; 24] = [
        "typescript",
        "javascript",
        "rust",
        "python",
        "json",
        "markdown",
        "css",
        "scss",
        "html",
        "xml",
        "yaml",
        "toml",
        "shell",
        "go",
        "java",
        "kotlin",
        "swift",
        "c",
        "cpp",
        "csharp",
        "ruby",
        "php",
        "sql",
        "dockerfile",
    ];

    fn run(language: &str, text: &str) -> Highlighted {
        let syntax = syntax_for(language).expect("a grammar");
        highlight(text, syntax, syntax_set(), Instant::now() + SLOW, &go()).expect("highlighted")
    }

    /// A cancel flag that is not set.
    fn go() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// The class of the span covering exactly `token` on line `line`.
    fn class_of(text: &str, spans: &Highlighted, line: usize, token: &str) -> Option<TokenClass> {
        let source = text.lines().nth(line)?;
        let at = source.find(token)?;
        spans[line]
            .iter()
            .find(|(range, _)| range.start <= at && at + token.len() <= range.end)
            .map(|(_, class)| *class)
    }

    fn classes(spans: &Highlighted) -> Vec<TokenClass> {
        spans.iter().flatten().map(|(_, class)| *class).collect()
    }

    #[test]
    fn every_daemon_language_has_a_grammar() {
        for id in IDS {
            assert!(syntax_for(id).is_some(), "{id}");
        }
        assert_eq!(
            syntax_for("dockerfile").map(|s| s.name.as_str()),
            Some("Dockerfile")
        );
        assert_eq!(syntax_for("rust").map(|s| s.name.as_str()), Some("Rust"));
    }

    #[test]
    fn plaintext_and_unknown_ids_have_none() {
        assert!(syntax_for("plaintext").is_none());
        assert!(
            syntax_for("powershell").is_none(),
            "not in fancy-regex two-face"
        );
        assert!(syntax_for("klingon").is_none());
        assert!(syntax_for("").is_none());
    }

    #[test]
    fn rust_tokens_are_classified() {
        let text = "fn main() {\n    let s = \"hi\";\n    // x\n}\n";
        let spans = run("rust", text);
        assert_eq!(spans.len(), 4, "one entry a line");
        assert_eq!(class_of(text, &spans, 0, "fn"), Some(TokenClass::Keyword));
        assert_eq!(
            class_of(text, &spans, 0, "main"),
            Some(TokenClass::Function)
        );
        assert_eq!(
            class_of(text, &spans, 1, "\"hi\""),
            Some(TokenClass::String)
        );
        assert_eq!(class_of(text, &spans, 2, "// x"), Some(TokenClass::Comment));
    }

    #[test]
    fn spans_stop_at_the_line_ending_and_stay_on_char_boundaries() {
        let text = "// é日\r\nlet x = 1;\r\n";
        let spans = run("rust", text);
        let first = text.lines().next().expect("a line").trim_end_matches('\r');
        for (range, _) in &spans[0] {
            assert!(range.end <= first.len(), "{range:?}");
            assert!(first.is_char_boundary(range.start) && first.is_char_boundary(range.end));
        }
    }

    #[test]
    fn toml_and_typescript_are_covered() {
        let toml = run("toml", "[package]\nname = \"x\"\nversion = 3\nok = true\n");
        let found = classes(&toml);
        assert!(found.contains(&TokenClass::String), "{toml:?}");
        assert!(
            found.contains(&TokenClass::Keyword) || found.contains(&TokenClass::Constant),
            "{toml:?}"
        );
        let ts = run(
            "typescript",
            "const x: number = 1;\nexport function f() { return 'a'; }\n",
        );
        let found = classes(&ts);
        assert!(found.contains(&TokenClass::String), "{ts:?}");
        assert!(
            found.contains(&TokenClass::Keyword) || found.contains(&TokenClass::Constant),
            "{ts:?}"
        );
    }

    #[test]
    fn too_many_lines_is_left_uncoloured() {
        let syntax = syntax_for("rust").expect("rust");
        let deadline = Instant::now() + SLOW;
        let at_limit = "x\n".repeat(MAX_LINES);
        assert!(highlight(&at_limit, syntax, syntax_set(), deadline, &go()).is_ok());
        let over = "x\n".repeat(MAX_LINES + 1);
        assert_eq!(
            highlight(&over, syntax, syntax_set(), deadline, &go()),
            Err(Uncoloured::TooManyLines)
        );
    }

    #[test]
    fn a_long_line_is_left_uncoloured() {
        let syntax = syntax_for("rust").expect("rust");
        let deadline = Instant::now() + SLOW;
        let at_limit = format!("a\n{}\r\n", "x".repeat(MAX_LINE_BYTES));
        assert!(highlight(&at_limit, syntax, syntax_set(), deadline, &go()).is_ok());
        let over = format!("a\n{}\n", "x".repeat(MAX_LINE_BYTES + 1));
        assert_eq!(
            highlight(&over, syntax, syntax_set(), deadline, &go()),
            Err(Uncoloured::LineTooLong)
        );
    }

    #[test]
    fn a_passed_deadline_gives_up_with_nothing() {
        let syntax = syntax_for("rust").expect("rust");
        let text = "fn a() {}\nfn b() {}\n";
        let now = Instant::now();
        assert!(highlight(text, syntax, syntax_set(), now + SLOW, &go()).is_ok());
        assert_eq!(
            highlight(text, syntax, syntax_set(), now, &go()),
            Err(Uncoloured::TimedOut)
        );
    }

    #[test]
    fn a_set_cancel_flag_gives_up_with_nothing() {
        let syntax = syntax_for("rust").expect("rust");
        let text = "fn a() {}\n".repeat(MAX_LINES);
        let started = Instant::now();
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            highlight(&text, syntax, syntax_set(), started + SLOW, &cancelled),
            Err(Uncoloured::Cancelled)
        );
        assert!(started.elapsed() < TIME_LIMIT, "stops at once");
    }

    #[test]
    fn every_scope_rule_parses() {
        assert_eq!(classifier().len(), RULES.len());
    }

    #[test]
    fn only_mapped_languages_have_a_grammar_without_loading_it() {
        for id in IDS {
            assert!(has_grammar(id), "{id}");
        }
        for id in ["plaintext", "powershell", "klingon", ""] {
            assert!(!has_grammar(id), "{id}");
        }
    }

    #[test]
    fn neighbouring_spans_of_one_class_merge() {
        let mut spans = LineSpans::new();
        push(&mut spans, 0..2, Some(TokenClass::String));
        push(&mut spans, 2..5, Some(TokenClass::String));
        push(&mut spans, 5..6, None);
        push(&mut spans, 6..6, Some(TokenClass::Keyword));
        push(&mut spans, 6..8, Some(TokenClass::String));
        push(&mut spans, 8..9, Some(TokenClass::Keyword));
        assert_eq!(
            spans,
            [
                (0..5, TokenClass::String),
                (6..8, TokenClass::String),
                (8..9, TokenClass::Keyword),
            ]
        );
        let text = "let s = \"abc\";\n";
        let strings: Vec<_> = run("rust", text)[0]
            .iter()
            .filter(|(_, class)| *class == TokenClass::String)
            .cloned()
            .collect();
        assert_eq!(strings, [(8..13, TokenClass::String)], "quotes and body");
    }

    #[test]
    fn a_palette_override_changes_the_class_colour() {
        let background = theme::DEFAULT_BACKGROUND;
        let palette = theme::build_theme(background).ansi;
        let mut custom = palette;
        custom[5] = Rgb {
            r: 0xff,
            g: 0x40,
            b: 0x40,
        };
        let base = class_colors(&palette, background);
        let changed = class_colors(&custom, background);
        let keyword = TokenClass::Keyword.index();
        assert_ne!(base[keyword], changed[keyword]);
        assert_eq!(changed[keyword], custom[5], "already legible, kept as is");
        for class in TokenClass::ALL {
            if class != TokenClass::Keyword {
                assert_eq!(base[class.index()], changed[class.index()], "{class:?}");
            }
        }
    }

    #[test]
    fn every_class_colour_clears_the_contrast_minimum() {
        let background = theme::DEFAULT_BACKGROUND;
        let dark = Rgb { r: 0, g: 0, b: 0 };
        for palette in [theme::build_theme(background).ansi, [dark; 16]] {
            for color in class_colors(&palette, background) {
                let ratio = theme::contrast_ratio(color, background);
                assert!(ratio >= MIN_CONTRAST, "{color:?}: {ratio}");
            }
        }
    }
}
