//! A shell tool's command split into its statements, one per line, for an
//! open tool card. It splits at the command's own newlines and at top-level
//! `;`, `&&` and `||`, each operator kept at the end of its statement. It
//! never splits inside quotes, `$(…)`, `$((…))`, backticks, `${…}`, a
//! POSIX `{ …; }` group, a PowerShell `{…}` script block or here-string, or
//! a heredoc's body. A pipe stays inside its statement, and a line that
//! starts with `|` continues the line before; any other operator that
//! starts a line stays at the start of its statement. No text is dropped.
//! Plain Rust: no GPUI and no I/O.

use serde_json::Value;

/// Which quoting rules a command follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellFamily {
    /// A POSIX shell: `'…'`, `"…"`, backslash escapes, backticks, `${…}`
    /// and heredocs.
    Posix,
    /// PowerShell: `'…'` with `''`, `"…"` with backtick escapes and `""`,
    /// `{…}` script blocks and `@'…'@` / `@"…"@` here-strings.
    PowerShell,
}

impl ShellFamily {
    /// The family of the shell tool named `tool`, or `None` for a tool that
    /// runs no shell command.
    pub(crate) fn of_tool(tool: &str) -> Option<Self> {
        match tool {
            "Bash" => Some(Self::Posix),
            "PowerShell" => Some(Self::PowerShell),
            _ => None,
        }
    }
}

/// The statements an open card of tool `name` lists: its input JSON's
/// `command` split by the tool's shell family. An input cut short by the
/// daemon's size cap no longer parses, and then the card's one-line
/// `summary` is split instead. Empty for a tool that runs no shell command,
/// or when the summary names only the tool.
pub(crate) fn tool_statements(name: &str, summary: &str, input: &str) -> Vec<String> {
    let Some(family) = ShellFamily::of_tool(name) else {
        return Vec::new();
    };
    match command_in(input) {
        Some(command) => split_statements(&command, family),
        None if summary.trim() == name => Vec::new(),
        None => split_statements(summary, family),
    }
}

/// The `command` string of a tool's input JSON; `None` when the JSON was cut
/// short (it no longer parses) or carries no command.
fn command_in(input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input).ok()?;
    value.get("command")?.as_str().map(str::to_owned)
}

/// `command` split into its statements, each trimmed, in order; blank
/// statements are dropped. An unterminated quote or bracket holds the rest
/// of the command in its statement.
pub(crate) fn split_statements(command: &str, family: ShellFamily) -> Vec<String> {
    let mut splitter = Splitter {
        chars: command.chars().collect(),
        family,
        nests: Vec::new(),
        heredocs: Vec::new(),
        start: 0,
        out: Vec::new(),
    };
    let mut at = 0;
    while at < splitter.chars.len() {
        at = splitter.step(at);
    }
    let end = splitter.chars.len();
    splitter.end_statement(end, end);
    splitter.out
}

/// What the scanner is inside of, innermost last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nest {
    Single,
    Double,
    Backtick,
    Paren,
    Brace,
    /// A POSIX `{ …; }` group, closed by a `}` word.
    Group,
    /// POSIX arithmetic, `$((…))` or `((…))`.
    Arith,
    /// A PowerShell here-string, closed by this quote then `@` at a line's
    /// start.
    HereString(char),
}

/// A heredoc whose body starts after the current line.
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

struct Splitter {
    chars: Vec<char>,
    family: ShellFamily,
    nests: Vec<Nest>,
    heredocs: Vec<Heredoc>,
    /// Where the statement being read starts.
    start: usize,
    out: Vec<String>,
}

impl Splitter {
    fn at(&self, index: usize) -> Option<char> {
        self.chars.get(index).copied()
    }

    fn posix(&self) -> bool {
        self.family == ShellFamily::Posix
    }

    /// The character that escapes the next one outside single quotes.
    fn escape(&self) -> char {
        if self.posix() { '\\' } else { '`' }
    }

    /// Reads from `at` and returns where to read next.
    fn step(&mut self, at: usize) -> usize {
        match self.nests.last().copied() {
            Some(Nest::Single) => self.in_single(at),
            Some(Nest::Double) => self.in_double(at),
            Some(Nest::Backtick) => self.in_backtick(at),
            Some(Nest::HereString(quote)) => self.in_here_string(at, quote),
            Some(Nest::Paren | Nest::Brace | Nest::Group | Nest::Arith) | None => self.in_code(at),
        }
    }

    fn in_single(&mut self, at: usize) -> usize {
        if self.chars[at] != '\'' {
            return at + 1;
        }
        if !self.posix() && self.at(at + 1) == Some('\'') {
            return at + 2;
        }
        self.nests.pop();
        at + 1
    }

    fn in_double(&mut self, at: usize) -> usize {
        match self.chars[at] {
            c if c == self.escape() => at + 2,
            '"' if !self.posix() && self.at(at + 1) == Some('"') => at + 2,
            '"' => {
                self.nests.pop();
                at + 1
            }
            '`' => {
                self.nests.push(Nest::Backtick);
                at + 1
            }
            '$' if self.at(at + 1) == Some('(') => {
                self.nests.push(Nest::Paren);
                at + 2
            }
            _ => at + 1,
        }
    }

    fn in_backtick(&mut self, at: usize) -> usize {
        match self.chars[at] {
            '\\' => at + 2,
            '`' => {
                self.nests.pop();
                at + 1
            }
            _ => at + 1,
        }
    }

    fn in_here_string(&mut self, at: usize, quote: char) -> usize {
        let line_start = at == 0 || self.chars[at - 1] == '\n';
        if line_start && self.chars[at] == quote && self.at(at + 1) == Some('@') {
            self.nests.pop();
            return at + 2;
        }
        at + 1
    }

    /// Outside any quote: escapes, openers, comments, newlines and, at the
    /// top level, the operators that end a statement.
    fn in_code(&mut self, at: usize) -> usize {
        let c = self.chars[at];
        if c == self.escape() {
            return at + 2;
        }
        if let Some(next) = self.open_nest(at) {
            return next;
        }
        if let Some(next) = self.skip_comment(at) {
            return next;
        }
        if c == '\n' {
            return self.newline(at);
        }
        if self.nests.is_empty()
            && let Some(len) = self.operator_len(at)
        {
            if self.statement_has_words(at) {
                self.end_statement(at + len, at + len);
            }
            return at + len;
        }
        at + 1
    }

    /// Whether the statement read so far holds anything before `at`; an
    /// operator that starts a statement stays at its start.
    fn statement_has_words(&self, at: usize) -> bool {
        self.chars[self.start..at]
            .iter()
            .any(|c| !c.is_whitespace())
    }

    /// Whether `at` starts a word: the command's start, or after whitespace,
    /// `;`, `&`, `|` or `(`.
    fn word_start(&self, at: usize) -> bool {
        at == 0 || {
            let before = self.chars[at - 1];
            before.is_whitespace() || matches!(before, ';' | '&' | '|' | '(')
        }
    }

    /// Whether a POSIX `{` group opens at `at`: a `{` word on its own.
    fn group_opens(&self, at: usize) -> bool {
        self.posix() && self.word_start(at) && self.at(at + 1).is_none_or(char::is_whitespace)
    }

    /// Closes the innermost bracket at `at` when the character there closes
    /// it: `)` a `(`, `))` arithmetic, `}` a `${` or PowerShell `{`, and a
    /// `}` word a POSIX group. Anything else reads as text.
    fn close_nest(&mut self, at: usize) -> usize {
        let closes = match (self.nests.last(), self.chars[at]) {
            (Some(Nest::Arith), ')') if self.at(at + 1) == Some(')') => {
                self.nests.pop();
                return at + 2;
            }
            (Some(Nest::Paren), ')') | (Some(Nest::Brace), '}') => true,
            (Some(Nest::Group), '}') => {
                let before = self.chars[at - 1];
                before.is_whitespace() || matches!(before, ';' | '&')
            }
            _ => false,
        };
        if closes {
            self.nests.pop();
        }
        at + 1
    }

    /// Opens or closes a quote or bracket at `at`, or reads a heredoc's
    /// operator; returns where to read next, or `None` when `at` does
    /// neither.
    fn open_nest(&mut self, at: usize) -> Option<usize> {
        let next = self.at(at + 1);
        let (nest, len) = match self.chars[at] {
            '\'' => (Nest::Single, 1),
            '"' => (Nest::Double, 1),
            '`' if self.posix() => (Nest::Backtick, 1),
            '(' if self.posix() && next == Some('(') => (Nest::Arith, 2),
            '(' => (Nest::Paren, 1),
            '$' if self.posix() && next == Some('{') => (Nest::Brace, 2),
            '{' if !self.posix() => (Nest::Brace, 1),
            '{' if self.group_opens(at) => (Nest::Group, 1),
            '@' if self.here_string_opens(at) => (Nest::HereString(next?), 2),
            ')' | '}' => return Some(self.close_nest(at)),
            '<' if self.heredoc_may_open(at) => return Some(self.heredoc(at)),
            _ => return None,
        };
        self.nests.push(nest);
        Some(at + len)
    }

    /// Whether `<<` at `at` may open a heredoc: in a POSIX shell, outside
    /// arithmetic, where it is a shift.
    fn heredoc_may_open(&self, at: usize) -> bool {
        self.posix() && self.at(at + 1) == Some('<') && !self.nests.contains(&Nest::Arith)
    }

    /// Whether a PowerShell here-string opens at `at`: `@'` or `@"` with
    /// nothing after it on its line.
    fn here_string_opens(&self, at: usize) -> bool {
        if self.posix() || !matches!(self.at(at + 1), Some('\'' | '"')) {
            return false;
        }
        self.chars[at + 2..]
            .iter()
            .take_while(|&&c| c != '\n')
            .all(|c| c.is_whitespace())
    }

    /// Past a comment starting at `at` (`#` at a word's start, or a
    /// PowerShell `<#…#>` block), or `None` when none starts there.
    fn skip_comment(&self, at: usize) -> Option<usize> {
        let c = self.chars[at];
        if !self.posix() && c == '<' && self.at(at + 1) == Some('#') {
            let rest = self.chars[at + 2..]
                .windows(2)
                .position(|pair| pair == ['#', '>']);
            return Some(rest.map_or(self.chars.len(), |offset| at + 2 + offset + 2));
        }
        if c != '#' || !self.word_start(at) {
            return None;
        }
        let rest = self.chars[at..].iter().position(|&c| c == '\n');
        Some(rest.map_or(self.chars.len(), |offset| at + offset))
    }

    /// A newline at `at`: any heredoc bodies it starts are read into the
    /// statement (a heredoc that never closes reads as text), and at the top
    /// level the statement ends there unless the next line starts with `|`.
    fn newline(&mut self, at: usize) -> usize {
        let mut end = at;
        for doc in std::mem::take(&mut self.heredocs) {
            match self.heredoc_end(end + 1, &doc) {
                Some(closing) => end = closing,
                None => break,
            }
        }
        if self.nests.is_empty() && !self.pipe_continues(end + 1) {
            self.end_statement(end, end + 1);
        }
        end + 1
    }

    /// Whether the line starting at `from` opens with a pipe (`|`, not
    /// `||`), continuing the line before.
    fn pipe_continues(&self, from: usize) -> bool {
        let Some(first) = (from..self.chars.len()).find(|&i| !matches!(self.chars[i], ' ' | '\t'))
        else {
            return false;
        };
        self.chars[first] == '|' && self.at(first + 1) != Some('|')
    }

    /// Where the newline after `doc`'s closing line is, reading its body
    /// from `from`; `None` when the body never closes.
    fn heredoc_end(&self, from: usize, doc: &Heredoc) -> Option<usize> {
        let mut line_start = from;
        while line_start < self.chars.len() {
            let line_end = self.chars[line_start..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(self.chars.len(), |offset| line_start + offset);
            let line: String = self.chars[line_start..line_end].iter().collect();
            let line = line.trim_end_matches('\r');
            let line = if doc.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if line == doc.delimiter {
                return Some(line_end);
            }
            line_start = line_end + 1;
        }
        None
    }

    /// Reads a heredoc operator `<<` or `<<-` at `at` and its delimiter
    /// word; `<<<` is a here-string, whose word is read as code.
    fn heredoc(&mut self, at: usize) -> usize {
        let mut next = at + 2;
        if self.at(next) == Some('<') {
            return next + 1;
        }
        let strip_tabs = self.at(next) == Some('-');
        if strip_tabs {
            next += 1;
        }
        while matches!(self.at(next), Some(' ' | '\t')) {
            next += 1;
        }
        let (delimiter, after) = self.heredoc_word(next);
        if !delimiter.is_empty() {
            self.heredocs.push(Heredoc {
                delimiter,
                strip_tabs,
            });
        }
        after
    }

    /// A heredoc's delimiter word from `from` with its quotes and
    /// backslashes removed, and where the word ends.
    fn heredoc_word(&self, from: usize) -> (String, usize) {
        let mut word = String::new();
        let mut quote = None;
        let mut next = from;
        while let Some(c) = self.at(next) {
            match quote {
                _ if c == '\n' => break,
                Some(open) if c == open => quote = None,
                None if c == '\'' || c == '"' => quote = Some(c),
                None if c == '\\' => {}
                None if c.is_whitespace() || ";|&<>()".contains(c) => break,
                Some(_) | None => word.push(c),
            }
            next += 1;
        }
        (word, next)
    }

    /// The length of the statement-ending operator at `at`: `;` (or a
    /// `case` arm's `;;`), `&&` or `||`.
    fn operator_len(&self, at: usize) -> Option<usize> {
        match (self.chars[at], self.at(at + 1)) {
            (';', Some(';')) | ('&', Some('&')) | ('|', Some('|')) => Some(2),
            (';', _) => Some(1),
            _ => None,
        }
    }

    /// Ends the statement read so far at `end` and starts the next at
    /// `next`.
    fn end_statement(&mut self, end: usize, next: usize) {
        if self.start < end {
            let text: String = self.chars[self.start..end].iter().collect();
            let text = text.trim();
            if !text.is_empty() {
                self.out.push(text.to_owned());
            }
        }
        self.start = next;
    }
}

#[cfg(test)]
mod tests {
    use super::{ShellFamily, split_statements};

    fn posix(command: &str) -> Vec<String> {
        split_statements(command, ShellFamily::Posix)
    }

    fn pwsh(command: &str) -> Vec<String> {
        split_statements(command, ShellFamily::PowerShell)
    }

    #[test]
    fn splits_at_semicolons_and_operators_keeping_the_operator() {
        assert_eq!(posix("a; b && c || d"), ["a;", "b &&", "c ||", "d"]);
        assert_eq!(posix("a\nb\r\n\nc"), ["a", "b", "c"]);
    }

    #[test]
    fn never_splits_inside_quotes_command_substitution_backticks_or_a_heredoc() {
        assert_eq!(
            posix(r#"echo 'a; b' "c && d""#),
            [r#"echo 'a; b' "c && d""#]
        );
        assert_eq!(
            posix("x=$(cd y; pwd || echo \")\"); echo $x"),
            ["x=$(cd y; pwd || echo \")\");", "echo $x"]
        );
        assert_eq!(posix("echo `a; b` && c"), ["echo `a; b` &&", "c"]);
        assert_eq!(posix(r"echo a\; b"), [r"echo a\; b"]);
        assert_eq!(posix("echo ${x:-a;b}; y"), ["echo ${x:-a;b};", "y"]);
        assert_eq!(posix("ls # one; two\npwd"), ["ls # one; two", "pwd"]);
        assert_eq!(
            posix("cat <<'EOF' > f\na; b\n\nEOF\necho done"),
            ["cat <<'EOF' > f\na; b\n\nEOF", "echo done"]
        );
        assert_eq!(
            posix("cat <<-END\n\tx && y\n\tEND\nz"),
            ["cat <<-END\n\tx && y\n\tEND", "z"]
        );
    }

    #[test]
    fn an_unterminated_quote_stays_one_statement() {
        assert_eq!(posix("echo 'a; b && c"), ["echo 'a; b && c"]);
    }

    #[test]
    fn keeps_a_pipe_on_its_line() {
        assert_eq!(
            posix("a | b; c |& d 2>&1 | e"),
            ["a | b;", "c |& d 2>&1 | e"]
        );
    }

    #[test]
    fn splits_powershell_at_semicolons_not_inside_here_strings_or_backtick_escapes() {
        let command = "Get-Item x; Write-Host a`;b \"c;`\"d\" 'e;''f'; \
                       @'\none; two\n'@ | Out-File g; if ($y) { a; b } && c";
        assert_eq!(
            pwsh(command),
            [
                "Get-Item x;",
                "Write-Host a`;b \"c;`\"d\" 'e;''f';",
                "@'\none; two\n'@ | Out-File g;",
                "if ($y) { a; b } &&",
                "c",
            ]
        );
        assert_eq!(
            pwsh("$s = @\"\nx; $(y; z)\n\"@; w"),
            ["$s = @\"\nx; $(y; z)\n\"@;", "w"]
        );
    }

    #[test]
    fn the_owner_example_splits_into_its_seven_statements() {
        let command = "uv run --project $HOME/.claude/tools/linear python -m linear release add \
                       rustling-tulip RT-680 2>&1; uv run --project $HOME/.claude/tools/linear python \
                       -m linear snapshot rustling-tulip --repo-root . 2>&1 | tail -n 1; git add \
                       docs/plans/MAIN.md && pwsh -NoProfile -File tools/gate.ps1 run --log \
                       .tmp/commit.log --slot light --timeout 1200 -- git commit -F \
                       .tmp/msg-snapshot.txt > .tmp/csnap3.out 2>&1; echo rc=$?; git log --oneline -1; \
                       bash ~/.claude/tools/context-usage/context-usage.sh";
        assert_eq!(
            posix(command),
            [
                "uv run --project $HOME/.claude/tools/linear python -m linear release add \
                 rustling-tulip RT-680 2>&1;",
                "uv run --project $HOME/.claude/tools/linear python -m linear snapshot \
                 rustling-tulip --repo-root . 2>&1 | tail -n 1;",
                "git add docs/plans/MAIN.md &&",
                "pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/commit.log --slot light \
                 --timeout 1200 -- git commit -F .tmp/msg-snapshot.txt > .tmp/csnap3.out 2>&1;",
                "echo rc=$?;",
                "git log --oneline -1;",
                "bash ~/.claude/tools/context-usage/context-usage.sh",
            ]
        );
    }

    #[test]
    fn an_empty_or_blank_command_has_no_statements() {
        let none: [&str; 0] = [];
        assert_eq!(posix(""), none);
        assert_eq!(posix("  \n \t\r\n"), none);
        assert_eq!(pwsh(""), none);
    }

    #[test]
    fn a_trailing_operator_stays_on_its_statement() {
        assert_eq!(posix("make &&"), ["make &&"]);
        assert_eq!(posix("make;"), ["make;"]);
        assert_eq!(pwsh("Build ||  "), ["Build ||"]);
    }

    #[test]
    fn an_operator_starting_a_line_is_kept() {
        assert_eq!(posix("a\n|| b\n  && c"), ["a", "|| b", "&& c"]);
        assert_eq!(posix("a\n; b"), ["a", "; b"]);
        assert_eq!(posix("  \n ; \n"), [";"]);
        assert_eq!(posix("a\n  | b"), ["a\n  | b"]);
        assert_eq!(
            pwsh("Get-ChildItem\n| Where-Object x"),
            ["Get-ChildItem\n| Where-Object x"]
        );
    }

    #[test]
    fn a_posix_brace_group_stays_one_statement() {
        assert_eq!(
            posix("make || { echo fail; exit 1; }"),
            ["make ||", "{ echo fail; exit 1; }"]
        );
        assert_eq!(posix("{ a; b; } && c"), ["{ a; b; } &&", "c"]);
        assert_eq!(
            posix("echo {a,b}; rm {} x; c"),
            ["echo {a,b};", "rm {} x;", "c"]
        );
        assert_eq!(posix("{ echo a}; b; }; c"), ["{ echo a}; b; };", "c"]);
    }

    #[test]
    fn an_arithmetic_shift_is_not_a_heredoc() {
        assert_eq!(
            posix("echo $((1 << 4))\necho a; echo b"),
            ["echo $((1 << 4))", "echo a;", "echo b"]
        );
        assert_eq!(
            posix("((x = (1 << 2) + 1)); echo $x"),
            ["((x = (1 << 2) + 1));", "echo $x"]
        );
    }

    #[test]
    fn an_unterminated_heredoc_does_not_swallow_the_rest() {
        assert_eq!(posix("cat <<EOF\na; b"), ["cat <<EOF", "a;", "b"]);
    }
}
