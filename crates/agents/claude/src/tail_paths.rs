//! The files a shell tool call writes or names as its log: redirect targets
//! (`>`, `>>`, `2>`, `*>`, `&>`), the gate wrapper's and `rt.ps1`'s
//! `--log <path>`, `tee [-a] <path>` and `Tee-Object -FilePath <path>`, each
//! resolved against the session's working folder as moved by any `cd`,
//! `pushd` or `Set-Location` before it. A target whose text a variable, a
//! tilde, a backtick or a substitution decides is skipped, as are the null
//! devices and fd-only redirects.

use std::path::Path;

/// The shell a tool call's command is written for, which sets its quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shell {
    /// Bash and the other POSIX shells: `\` escapes.
    Posix,
    /// PowerShell: `` ` `` escapes and `''` / `""` double a quote.
    PowerShell,
}

impl Shell {
    /// The shell of a tool by its name, `None` for a tool that runs no shell.
    pub(crate) fn of_tool(name: &str) -> Option<Self> {
        match name {
            "Bash" => Some(Self::Posix),
            "PowerShell" => Some(Self::PowerShell),
            _ => None,
        }
    }

    fn escape(self) -> char {
        match self {
            Self::Posix => '\\',
            Self::PowerShell => '`',
        }
    }
}

/// A word with its quotes removed. `expands` marks one whose text a `$`
/// expansion decides, so its text is not the path the shell would use.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    text: String,
    expands: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(Word),
    /// A redirect that writes to the next word.
    WriteTo,
    /// A redirect that reads the next word.
    ReadFrom,
    /// A redirect that names no file, such as `2>&1`.
    FdOnly,
    /// The end of a simple command: `|`, `||`, `&&`, `;`, `&`, a newline or
    /// a parenthesis.
    Break,
}

/// The files `command` writes or names as a log, in the order it names
/// them and each once: absolute paths, or joined onto `cwd` when relative;
/// a relative path stays as written when `cwd` is unknown. A `cd`, `pushd`
/// or `Set-Location` moves the folder later relative paths are joined onto;
/// after one whose target cannot be told (`cd -`, `cd ~`, a variable),
/// relative paths are dropped.
pub(crate) fn tail_paths(shell: Shell, command: &str, cwd: Option<&str>) -> Vec<String> {
    let tokens = Lexer::new(command, shell).run();
    let mut paths: Vec<String> = Vec::new();
    let mut dir = Dir::Known(cwd.map(str::to_owned));
    for segment in tokens.split(|token| *token == Token::Break) {
        let (mut targets, words) = split_segment(segment);
        targets.extend(command_targets(shell, &words));
        for word in targets {
            if let Some(path) = resolve(shell, &word, &dir)
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
        if let Some(target) = location_target(shell, &words) {
            dir = dir.moved(shell, target);
        }
    }
    paths
}

/// The folder relative targets are joined onto.
enum Dir {
    /// The folder, or `None` when the caller gave none and relative targets
    /// stay as written.
    Known(Option<String>),
    /// A change of folder this cannot follow.
    Lost,
}

impl Dir {
    /// The folder after `change`.
    fn moved(self, shell: Shell, change: Move) -> Self {
        match change {
            Move::To(target) if target.text != "-" => {
                resolve(shell, &target, &self).map_or(Self::Lost, |path| Self::Known(Some(path)))
            }
            _ => Self::Lost,
        }
    }
}

/// A change of folder.
enum Move {
    /// To the folder a word names.
    To(Word),
    /// To a folder the command line does not name (the home folder).
    Unnamed,
}

/// One simple command's write redirect targets, and its other words.
fn split_segment(segment: &[Token]) -> (Vec<Word>, Vec<&Word>) {
    let mut targets = Vec::new();
    let mut words = Vec::new();
    let mut tokens = segment.iter();
    while let Some(token) = tokens.next() {
        match token {
            Token::WriteTo => {
                if let Some(Token::Word(word)) = tokens.next() {
                    targets.push(word.clone());
                }
            }
            Token::ReadFrom => {
                tokens.next();
            }
            Token::Word(word) => words.push(word),
            Token::FdOnly | Token::Break => {}
        }
    }
    (targets, words)
}

/// Where a `cd`, `pushd` or `Set-Location` command line moves to: `None`
/// for any other command.
fn location_target(shell: Shell, words: &[&Word]) -> Option<Move> {
    let (program, args) = words.split_first()?;
    let changes = matches!(
        program_name(&program.text).as_str(),
        "cd" | "chdir" | "pushd" | "set-location" | "sl" | "push-location"
    );
    if !changes {
        return None;
    }
    let target = match shell {
        Shell::Posix => posix_folder_arg(args).cloned(),
        Shell::PowerShell => location_path(args),
    };
    Some(target.map_or(Move::Unnamed, Move::To))
}

/// A POSIX `cd`'s folder: its first operand, `-` included.
fn posix_folder_arg<'w>(args: &[&'w Word]) -> Option<&'w Word> {
    let mut options_over = false;
    args.iter().copied().find(|arg| {
        if !options_over && arg.text == "--" {
            options_over = true;
            return false;
        }
        options_over || arg.text == "-" || !arg.text.starts_with('-')
    })
}

/// `Set-Location`'s folder: the value of `-Path` or `-LiteralPath` (as
/// `-Name value` or `-Name:value`), else its first positional argument.
fn location_path(args: &[&Word]) -> Option<Word> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        index += 1;
        let flag = match arg.text.strip_prefix('-') {
            Some(flag) if !flag.is_empty() => flag,
            _ => return Some((*arg).clone()),
        };
        let (name, inline) = match flag.split_once(':') {
            Some((name, value)) => (name.to_ascii_lowercase(), Some(value)),
            None => (flag.to_ascii_lowercase(), None),
        };
        if matches!(name.as_str(), "path" | "literalpath" | "lp") {
            return match inline {
                Some(value) => Some(Word {
                    text: value.to_owned(),
                    expands: arg.expands,
                }),
                None => args.get(index).map(|value| (*value).clone()),
            };
        }
        index += usize::from(name == "stackname" && inline.is_none());
    }
    None
}

/// The paths a command line names: a `--log` value, `tee`'s files and
/// `Tee-Object`'s file.
fn command_targets(shell: Shell, words: &[&Word]) -> Vec<Word> {
    let start = words
        .iter()
        .position(|word| !(shell == Shell::Posix && is_assignment(&word.text)))
        .unwrap_or(words.len());
    let Some((program, args)) = words[start..].split_first() else {
        return Vec::new();
    };
    let mut targets = known_log_values(program, args);
    match (program_name(&program.text).as_str(), shell) {
        ("tee", Shell::Posix) => targets.extend(tee_files(args)),
        ("tee" | "tee-object", Shell::PowerShell) | ("tee-object", Shell::Posix) => {
            targets.extend(tee_object_file(args));
        }
        _ => {}
    }
    targets
}

/// `NAME=value` before a POSIX command.
fn is_assignment(text: &str) -> bool {
    text.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// A program's bare name, lowercased: no folder and no `.exe`.
fn program_name(text: &str) -> String {
    let base = text
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(text)
        .to_ascii_lowercase();
    base.strip_suffix(".exe")
        .map_or_else(|| base.clone(), str::to_owned)
}

/// The `--log` values of a program known to take a log file there: the gate
/// wrapper's `run` (`gate.ps1` or `gate`, before its `--`) and `rt.ps1`.
/// Any other program's `--log` may be a level or anything else.
fn known_log_values(program: &Word, args: &[&Word]) -> Vec<Word> {
    let (name, args) = script_of(program, args);
    match name.as_str() {
        "gate.ps1" | "gate" if args.first().is_some_and(|arg| arg.text == "run") => {
            let own = args
                .iter()
                .position(|arg| arg.text == "--")
                .map_or(args, |end| &args[..end]);
            log_flag_values(own)
        }
        "rt.ps1" => log_flag_values(args),
        _ => Vec::new(),
    }
}

/// The program a command line runs and its arguments: a `pwsh -File`
/// script stands for itself.
fn script_of<'a, 'w>(program: &Word, args: &'a [&'w Word]) -> (String, &'a [&'w Word]) {
    let name = program_name(&program.text);
    if matches!(name.as_str(), "pwsh" | "powershell")
        && let Some(at) = args
            .iter()
            .position(|arg| matches!(arg.text.to_ascii_lowercase().as_str(), "-file" | "-f"))
        && let Some(script) = args.get(at + 1)
    {
        return (program_name(&script.text), &args[at + 2..]);
    }
    (name, args)
}

fn log_flag_values(args: &[&Word]) -> Vec<Word> {
    let mut values = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if arg.text == "--log" {
            values.extend(args.get(index + 1).map(|value| (*value).clone()));
        } else if let Some(value) = arg.text.strip_prefix("--log=") {
            values.push(Word {
                text: value.to_owned(),
                expands: arg.expands,
            });
        }
    }
    values
}

/// POSIX `tee`'s files: every operand that is not an option.
fn tee_files(args: &[&Word]) -> Vec<Word> {
    let mut files = Vec::new();
    let mut options_over = false;
    for arg in args {
        if !options_over && arg.text == "--" {
            options_over = true;
        } else if options_over || !arg.text.starts_with('-') {
            files.push((*arg).clone());
        }
    }
    files
}

/// `Tee-Object`'s file: the value of `-FilePath`, `-LiteralPath` or `-Path`
/// (named in any case, as `-Name value` or `-Name:value`), else its first
/// positional argument; none with `-Variable`.
fn tee_object_file(args: &[&Word]) -> Option<Word> {
    let mut positional = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        index += 1;
        let Some(flag) = arg.text.strip_prefix('-') else {
            positional = positional.or(Some((*arg).clone()));
            continue;
        };
        let (name, inline) = match flag.split_once(':') {
            Some((name, value)) => (name.to_ascii_lowercase(), Some(value)),
            None => (flag.to_ascii_lowercase(), None),
        };
        let takes_value = matches!(name.as_str(), "variable" | "encoding" | "inputobject");
        if !matches!(name.as_str(), "filepath" | "literalpath" | "path") {
            if name == "variable" {
                return None;
            }
            index += usize::from(takes_value && inline.is_none());
            continue;
        }
        return match inline {
            Some(value) => Some(Word {
                text: value.to_owned(),
                expands: arg.expands,
            }),
            None => args.get(index).map(|value| (*value).clone()),
        };
    }
    positional
}

/// The path a target names, or `None` for a null device, an empty text,
/// one an expansion decides or a relative one after a lost `cd`.
fn resolve(shell: Shell, word: &Word, dir: &Dir) -> Option<String> {
    let text = word.text.as_str();
    if word.expands || text.is_empty() || text.starts_with('~') || is_null_device(text) {
        return None;
    }
    let text = if shell == Shell::Posix {
        drive_path(text)
    } else {
        text.to_owned()
    };
    let path = Path::new(&text);
    if path.is_absolute() {
        return Some(text);
    }
    match dir {
        Dir::Known(Some(cwd)) => Some(Path::new(cwd).join(path).display().to_string()),
        Dir::Known(None) => Some(text),
        Dir::Lost => None,
    }
}

fn is_null_device(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty" | "nul" | "nul:" | "$null"
    )
}

/// A Git Bash drive path (`/c/logs/x.log`) as Windows writes it
/// (`c:/logs/x.log`); any other text unchanged. Only on Windows.
fn drive_path(text: &str) -> String {
    let mut chars = text.chars();
    if cfg!(windows)
        && chars.next() == Some('/')
        && let Some(drive) = chars.next().filter(char::is_ascii_alphabetic)
        && matches!(chars.next(), None | Some('/'))
    {
        return format!("{drive}:/{}", chars.as_str());
    }
    text.to_owned()
}

/// Splits a command into [`Token`]s. Heredoc bodies and comments are
/// skipped.
struct Lexer {
    chars: Vec<char>,
    pos: usize,
    shell: Shell,
    tokens: Vec<Token>,
    /// Delimiters of heredocs whose bodies start after the next newline, and
    /// whether their lines may be indented with tabs (`<<-`).
    heredocs: Vec<(String, bool)>,
}

impl Lexer {
    fn new(command: &str, shell: Shell) -> Self {
        Self {
            chars: command.chars().collect(),
            pos: 0,
            shell,
            tokens: Vec::new(),
            heredocs: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<Token> {
        while let Some(c) = self.peek(0) {
            self.step(c);
        }
        self.tokens
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn step(&mut self, c: char) {
        match c {
            '\n' => {
                self.pos += 1;
                self.tokens.push(Token::Break);
                self.skip_heredoc_bodies();
            }
            '#' => self.skip_comment(),
            '|' | ';' | '(' | ')' => {
                self.pos += 1;
                self.tokens.push(Token::Break);
            }
            '&' => self.ampersand(),
            '<' => self.less(),
            '>' => self.greater(),
            c if c.is_whitespace() => self.pos += 1,
            _ if self.fd_prefix_len() > 0 => {
                self.pos += self.fd_prefix_len();
                self.greater();
            }
            _ => self.word(),
        }
    }

    /// The length of an fd number (`2`) or PowerShell's `*` standing right
    /// before a `>`, else 0.
    fn fd_prefix_len(&self) -> usize {
        let digits = self.chars[self.pos..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        let len = if digits == 0 && self.peek(0) == Some('*') {
            1
        } else {
            digits
        };
        if len > 0 && self.peek(len) == Some('>') {
            len
        } else {
            0
        }
    }

    fn skip_comment(&mut self) {
        while self.peek(0).is_some_and(|c| c != '\n') {
            self.pos += 1;
        }
    }

    /// `&&`, `&` (both end the command) or the `&>` redirect.
    fn ampersand(&mut self) {
        self.pos += 1;
        match self.peek(0) {
            Some('>') => self.greater(),
            Some('&') => {
                self.pos += 1;
                self.tokens.push(Token::Break);
            }
            _ => self.tokens.push(Token::Break),
        }
    }

    /// A write redirect at `>`: `>`, `>>`, `>|`, and `>&2` (fd only) or
    /// `>&file` (a file).
    fn greater(&mut self) {
        self.pos += 1;
        if matches!(self.peek(0), Some('>' | '|')) {
            self.pos += 1;
        }
        if self.peek(0) != Some('&') {
            self.tokens.push(Token::WriteTo);
            return;
        }
        self.pos += 1;
        if self.peek(0).is_some_and(|c| c.is_ascii_digit() || c == '-') {
            while self.peek(0).is_some_and(|c| c.is_ascii_digit() || c == '-') {
                self.pos += 1;
            }
            self.tokens.push(Token::FdOnly);
        } else {
            self.tokens.push(Token::WriteTo);
        }
    }

    /// A read redirect, a here-string, or a heredoc whose delimiter is read
    /// here and whose body is skipped after the next newline.
    fn less(&mut self) {
        self.pos += 1;
        if self.peek(0) != Some('<') {
            if self.peek(0) == Some('&') {
                self.pos += 1;
            }
            self.tokens.push(Token::ReadFrom);
            return;
        }
        self.pos += 1;
        if self.peek(0) == Some('<') {
            self.pos += 1;
            self.tokens.push(Token::ReadFrom);
            return;
        }
        let tabs = self.peek(0) == Some('-');
        if tabs {
            self.pos += 1;
        }
        while self.peek(0).is_some_and(|c| c == ' ' || c == '\t') {
            self.pos += 1;
        }
        let delimiter = self.read_word().text;
        if !delimiter.is_empty() {
            self.heredocs.push((delimiter, tabs));
        }
    }

    fn skip_heredoc_bodies(&mut self) {
        for (delimiter, tabs) in std::mem::take(&mut self.heredocs) {
            while self.pos < self.chars.len() {
                let line = self.take_line();
                let line = line.trim_end_matches('\r');
                let line = if tabs {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                if line == delimiter {
                    break;
                }
            }
        }
    }

    fn take_line(&mut self) -> String {
        let mut line = String::new();
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if c == '\n' {
                break;
            }
            line.push(c);
        }
        line
    }

    fn word(&mut self) {
        let word = self.read_word();
        self.tokens.push(Token::Word(word));
    }

    /// One word up to unquoted white space or an operator, its quotes and
    /// escapes removed.
    fn read_word(&mut self) -> Word {
        let mut word = Word {
            text: String::new(),
            expands: false,
        };
        while let Some(c) = self.peek(0) {
            if c.is_whitespace() || matches!(c, '|' | ';' | '&' | '<' | '>' | '(' | ')') {
                break;
            }
            self.pos += 1;
            match c {
                '\'' => self.single_quoted(&mut word.text),
                '"' => self.double_quoted(&mut word),
                '$' => {
                    word.expands |= self.expansion_follows();
                    word.text.push(c);
                }
                '`' => self.backtick(&mut word, Self::escaped),
                c if c == self.shell.escape() => self.escaped(&mut word.text),
                _ => word.text.push(c),
            }
        }
        word
    }

    /// A backtick outside single quotes: a POSIX command substitution, or
    /// PowerShell's escape, handled by `escape`. Either way the word is not
    /// taken as a path.
    fn backtick(&mut self, word: &mut Word, escape: fn(&mut Self, &mut String)) {
        word.expands = true;
        match self.shell {
            Shell::Posix => word.text.push('`'),
            Shell::PowerShell => escape(self, &mut word.text),
        }
    }

    /// The character after an escape outside quotes; an escaped newline
    /// joins the lines.
    fn escaped(&mut self, text: &mut String) {
        if let Some(next) = self.peek(0) {
            self.pos += 1;
            if next != '\n' {
                text.push(next);
            }
        }
    }

    /// Whether the `$` just read starts an expansion: a name, `{`, `(` or a
    /// special parameter follows.
    fn expansion_follows(&self) -> bool {
        self.peek(0)
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '{' | '(' | '?' | '@' | '*'))
    }

    fn single_quoted(&mut self, text: &mut String) {
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if c != '\'' {
                text.push(c);
            } else if self.shell == Shell::PowerShell && self.peek(0) == Some('\'') {
                self.pos += 1;
                text.push('\'');
            } else {
                return;
            }
        }
    }

    fn double_quoted(&mut self, word: &mut Word) {
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            match c {
                '"' if self.shell == Shell::PowerShell && self.peek(0) == Some('"') => {
                    self.pos += 1;
                    word.text.push('"');
                }
                '"' => return,
                '$' => {
                    word.expands |= self.expansion_follows();
                    word.text.push(c);
                }
                '`' => self.backtick(word, Self::escaped_in_double_quotes),
                c if c == self.shell.escape() => self.escaped_in_double_quotes(&mut word.text),
                _ => word.text.push(c),
            }
        }
    }

    /// POSIX keeps a `\` inside double quotes unless `"`, `\`, `$`, `` ` ``
    /// or a newline follows; PowerShell's `` ` `` always escapes.
    fn escaped_in_double_quotes(&mut self, text: &mut String) {
        let next = self.peek(0);
        let escapes =
            self.shell == Shell::PowerShell || matches!(next, Some('"' | '\\' | '$' | '`' | '\n'));
        if escapes {
            self.escaped(text);
        } else {
            text.push(self.shell.escape());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str) -> Vec<String> {
        tail_paths(Shell::Posix, command, None)
    }

    fn pwsh(command: &str) -> Vec<String> {
        tail_paths(Shell::PowerShell, command, None)
    }

    #[test]
    fn finds_redirect_targets() {
        assert_eq!(
            bash("cargo build > build.log 2> err.log; make >>all.log &> both.log"),
            ["build.log", "err.log", "all.log", "both.log"]
        );
        assert_eq!(bash("echo hi>out.txt"), ["out.txt"]);
        assert_eq!(bash("cmd >|forced.log"), ["forced.log"]);
        assert_eq!(
            pwsh("./build.ps1 *> all.log 3>> warn.log"),
            ["all.log", "warn.log"]
        );
        assert_eq!(bash("a > one.log && b > one.log"), ["one.log"]);
    }

    #[test]
    fn finds_a_log_flag_and_tee() {
        assert_eq!(
            pwsh(
                "pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/b.log --slot heavy -- cargo build"
            ),
            [".tmp/b.log"]
        );
        assert_eq!(bash("gate run --log=g.log -- make"), ["g.log"]);
        assert_eq!(pwsh(r".\rt.ps1 test --log t.log"), ["t.log"]);
        assert_eq!(bash("make 2>&1 | tee -a make.log"), ["make.log"]);
        assert_eq!(bash("make | /usr/bin/tee a.log b.log"), ["a.log", "b.log"]);
        assert_eq!(bash("RUST_LOG=debug tee -- -odd.log"), ["-odd.log"]);
    }

    #[test]
    fn finds_tee_object_filepath() {
        assert_eq!(pwsh("make | Tee-Object -FilePath make.log"), ["make.log"]);
        assert_eq!(pwsh("make | tee-object -filepath:make.log"), ["make.log"]);
        assert_eq!(
            pwsh("make | Tee-Object -Append -LiteralPath a.log"),
            ["a.log"]
        );
        assert_eq!(pwsh("make | tee make.log"), ["make.log"]);
        assert_eq!(
            pwsh("make | Tee-Object -Variable out"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn ignores_null_devices_and_fd_redirects() {
        assert_eq!(bash("make > /dev/null 2>&1"), Vec::<String>::new());
        assert_eq!(bash("make 2>/dev/null >&2 1>&-"), Vec::<String>::new());
        assert_eq!(pwsh("make > $null 2>NUL *>&1"), Vec::<String>::new());
        assert_eq!(pwsh("make > nul"), Vec::<String>::new());
        assert_eq!(
            bash("make > \"$LOG\" 2> ${DIR}/e.log > ~/x.log"),
            Vec::<String>::new()
        );
        assert_eq!(bash("sort < input.txt; cat <<< hi"), Vec::<String>::new());
        assert_eq!(bash("echo '$HOME' > '$literal.log'"), ["$literal.log"]);
    }

    #[test]
    fn a_heredoc_body_and_a_comment_are_not_read_as_commands() {
        let command = "cat > notes.md <<'EOF'\nnot > a.log\nEOF\necho done > b.log # > c.log";
        assert_eq!(bash(command), ["notes.md", "b.log"]);
        assert_eq!(
            bash("cat <<-END\n\tx > y.log\n\tEND\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn finds_quoted_paths() {
        assert_eq!(bash(r#"make > "build output.log""#), ["build output.log"]);
        assert_eq!(bash("make > 'it''s.log'"), ["its.log"]);
        assert_eq!(bash(r"make > my\ log.txt"), ["my log.txt"]);
        assert_eq!(bash(r#"make > "C:\logs\b.log""#), [r"C:\logs\b.log"]);
        assert_eq!(pwsh(r"make > C:\logs\a.log"), [r"C:\logs\a.log"]);
        assert_eq!(pwsh("make > 'it''s.log'"), ["it's.log"]);
        assert_eq!(
            pwsh(r#"make | Tee-Object -FilePath "a ""b"".log""#),
            [r#"a "b".log"#]
        );
    }

    #[test]
    fn resolves_paths_against_the_working_folder() {
        let cwd = std::env::temp_dir().join("repo");
        let cwd_text = cwd.display().to_string();
        let found = tail_paths(Shell::Posix, "make > logs/b.log", Some(&cwd_text));
        assert_eq!(found, [cwd.join("logs/b.log").display().to_string()]);

        let absolute = std::env::temp_dir().join("elsewhere.log");
        let command = format!("make > \"{}\"", absolute.display());
        let found = tail_paths(Shell::PowerShell, &command, Some(&cwd_text));
        assert_eq!(found, [absolute.display().to_string()]);

        assert_eq!(
            bash("make > b.log"),
            ["b.log"],
            "no folder leaves it as written"
        );
    }

    #[test]
    fn a_cd_earlier_in_the_command_moves_relative_targets() {
        let cwd = std::env::temp_dir().join("repo");
        let cwd_text = cwd.display().to_string();
        let elsewhere = std::env::temp_dir().join("elsewhere");
        let in_bash = |command: &str| tail_paths(Shell::Posix, command, Some(&cwd_text));
        let in_pwsh = |command: &str| tail_paths(Shell::PowerShell, command, Some(&cwd_text));
        let under = |path: &Path, rel: &str| vec![path.join(rel).display().to_string()];

        let sub = cwd.join("sub");
        assert_eq!(in_bash("cd sub && make > a.log"), under(&sub, "a.log"));
        assert_eq!(in_bash("make > a.log; cd sub"), under(&cwd, "a.log"));
        assert_eq!(
            in_bash("pushd sub; cd deeper; make > b.log"),
            under(&sub.join("deeper"), "b.log")
        );
        let command = format!("cd \"{}\" && make > c.log", elsewhere.display());
        assert_eq!(in_bash(&command), under(&elsewhere, "c.log"));
        assert_eq!(
            in_pwsh("Set-Location -Path sub; make > d.log"),
            under(&sub, "d.log")
        );
        assert_eq!(
            in_pwsh("set-location sub; make *> e.log"),
            under(&sub, "e.log")
        );

        for lost in [
            "cd - && make > f.log",
            "cd && make > f.log",
            "cd $DIR && make > f.log",
            "cd ~/x; make > f.log",
        ] {
            assert_eq!(in_bash(lost), Vec::<String>::new(), "{lost}");
        }
        let absolute = std::env::temp_dir().join("abs.log");
        let command = format!("cd -; make > \"{}\"", absolute.display());
        assert_eq!(
            in_bash(&command),
            [absolute.display().to_string()],
            "an absolute target survives"
        );
    }

    #[test]
    fn a_log_flag_on_an_unknown_program_is_not_a_path() {
        assert_eq!(bash("server --log info"), Vec::<String>::new());
        assert_eq!(bash("server --port 1 --log=run.log"), Vec::<String>::new());
        assert_eq!(bash("gate status --log g.log"), Vec::<String>::new());
        assert_eq!(
            pwsh("pwsh -File tools/gate.ps1 run --log a.log -- cargo run -- --log info"),
            ["a.log"],
            "a --log after gate's `--` is the wrapped program's"
        );
    }

    #[test]
    fn a_backtick_target_is_dropped() {
        assert_eq!(bash("make > `date`.log"), Vec::<String>::new());
        assert_eq!(bash("make > \"run-`date`.log\""), Vec::<String>::new());
        assert_eq!(bash("make | tee `x`"), Vec::<String>::new());
        assert_eq!(bash("make > $(date).log"), Vec::<String>::new());
        assert_eq!(pwsh("make > a`b.log"), Vec::<String>::new());
        assert_eq!(bash("make > 'a`b.log'"), ["a`b.log"], "quoted literally");
    }

    #[cfg(windows)]
    #[test]
    fn a_git_bash_drive_path_becomes_a_windows_path() {
        assert_eq!(bash("make > /c/logs/a.log"), ["c:/logs/a.log"]);
    }
}
