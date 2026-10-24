//! Program resolution for spawns: the agent CLI (through npm shims on Windows, and the user's launch command) and the user's shell, with its prompt-integration scripts and init files.

use crate::paths::Dirs;
use crate::state::AgentCommand;
use agent_api::AgentPlugin;
use anyhow::Context as _;
use std::path::{Path, PathBuf};

/// Resolve an agent-program name into something `CreateProcess` /
/// portable-pty can actually launch. Returns `(program, prepended_args)`.
///
/// On Windows, agent CLIs distributed via npm (e.g. claude from
/// `@anthropic-ai/claude-code`) land on
/// PATH as `.cmd` shims. Two layers of fixup matter here:
///
/// 1. `CreateProcess` does NOT execute `.cmd` or `.bat` directly, so
///    a raw `.cmd` path passed to portable-pty silently fails (the
///    tracer's pipe never opens; daemon times out after 10s).
/// 2. Even wrapping with `cmd.exe /d /c <shim>.cmd` is enough for the
///    process to launch — but inside portable-pty's pseudo-console
///    that chain (cmd.exe → npm shim → node.exe → real binary) ends
///    up swallowing the child's stdout: the agent never renders, and
///    flags like `--dangerously-skip-permissions` look like they're
///    being ignored because the user sees no output at all. The same
///    `cmd.exe /d /c <shim>.cmd …` argv works fine in a real Windows
///    console and from `PowerShell`, so it's specific to the
///    portable-pty + cmd.exe boundary.
///
/// We dodge both by, when the `.cmd` is an npm-style shim, peeking
/// into the sibling `node_modules/.../bin/` directory and spawning
/// either the native `<stem>.exe` (newer claude-code distribution) or
/// `node.exe <stem>.js` (older claude) directly. That removes
/// cmd.exe from the chain entirely.
///
/// Order of preference for a `.cmd` shim:
///   1. native binary → `(<bin>/<stem>.exe, [...])`.
///   2. JS entrypoint → `(node.exe, [<bin>/<stem>.js, ...])`.
///   3. Fallback     → `(cmd.exe, ["/d", "/c", <shim-path>, ...])`.
///
/// Native `.exe`s and anything else flow through with the resolved
/// path unchanged. When `which` can't find the program at all (custom
/// env-var override that hasn't been deployed yet, etc.) the input
/// string is returned as-is and we let the tracer surface whatever
/// error `CreateProcess` produces — that's more useful diagnostics
/// than guessing.
pub(super) fn resolve_agent_program(name: &str) -> (String, Vec<String>) {
    let Ok(path) = which::which(name) else {
        return (name.to_string(), Vec::new());
    };
    let path_str = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(ext.as_deref(), Some("cmd" | "bat"))
            && let Some((program, prepend)) = npm_shim_to_native_program(&path)
        {
            return (program.to_string_lossy().into_owned(), prepend);
        }
        if matches!(ext.as_deref(), Some("cmd" | "bat")) {
            return (
                "cmd.exe".to_string(),
                vec!["/d".to_string(), "/c".to_string(), path_str],
            );
        }
    }
    (path_str, Vec::new())
}

/// For a `.cmd` path that looks like an npm-generated shim, locate
/// the underlying program the shim ultimately invokes. Two layouts
/// are supported, in this order of preference:
///
/// - Native binary distribution (`@anthropic-ai/claude-code` since
///   ~2026): `<dir>/node_modules/<pkg>/bin/<stem>.exe`. Returned as
///   `(<exe>, [])` — no node prefix.
/// - JS entrypoint (older claude): `<dir>/node_modules/<pkg>/
///   bin/<stem>.js`. Returned as `(node.exe, [<js>])`. The `node.exe`
///   resolution prefers a sibling next to the shim (nvm-style install
///   layout) over a PATH-resolved one, so node-version managers stay
///   self-consistent.
///
/// `<pkg>` may be scoped as `@org/name`.
///
/// Returns `None` if the shim doesn't match either pattern (custom
/// wrapper `.cmd`, non-npm CLI, etc.) — caller falls back to
/// `cmd.exe /d /c <shim>`.
#[cfg(windows)]
fn npm_shim_to_native_program(shim: &Path) -> Option<(PathBuf, Vec<String>)> {
    let dir = shim.parent()?;
    let stem = shim.file_stem()?.to_str()?;
    let nm = dir.join("node_modules");
    if !nm.is_dir() {
        return None;
    }

    let exe_filename = format!("{stem}.exe");
    let js_filename = format!("{stem}.js");

    let mut exe_candidates: Vec<PathBuf> = Vec::new();
    let mut js_candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&nm).ok()?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_str()?;
        if name_str.starts_with('@') {
            // Scoped: walk one more level (e.g. node_modules/@anthropic-ai/claude-code/bin/claude.js).
            if let Ok(inner) = std::fs::read_dir(&p) {
                for sub in inner.flatten() {
                    let bin = sub.path().join("bin");
                    exe_candidates.push(bin.join(&exe_filename));
                    js_candidates.push(bin.join(&js_filename));
                }
            }
        } else {
            // Unscoped: node_modules/<pkg>/bin/<stem>.{exe,js}.
            let bin = p.join("bin");
            exe_candidates.push(bin.join(&exe_filename));
            js_candidates.push(bin.join(&js_filename));
        }
    }

    if let Some(exe) = exe_candidates.into_iter().find(|c| c.is_file()) {
        return Some((exe, Vec::new()));
    }

    let js = js_candidates.into_iter().find(|c| c.is_file())?;
    let sibling_node = dir.join("node.exe");
    let node = if sibling_node.is_file() {
        sibling_node
    } else {
        which::which("node").ok()?
    };
    Some((node, vec![js.to_string_lossy().into_owned()]))
}

#[derive(Debug, Clone)]
pub(super) struct ShellProgram {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) label: String,
    pub(super) extra_env: Vec<(String, String)>,
}

/// Pick a shell executable for [`SessionMode::PlainShell`] spawns. `program`
/// is what we pass to `CommandBuilder` (resolved on PATH if it has no
/// separator); `label` is the short token shown in the UI; `args` and
/// `extra_env` together install the shell-integration init script for shells
/// that support it (PowerShell / bash / zsh) — emits OSC 7 for cwd and
/// OSC 133 + OSC 633 marks the frontend parses for the per-command chip UI.
/// cmd.exe gets a minimal OSC 7 prompt hook only (no command tracking).
///
/// Resolution order:
/// - `RUSTLING_TULIP_SHELL` env override (any platform);
/// - Windows: `pwsh.exe` → `powershell.exe` → `cmd.exe`;
/// - Unix: `$SHELL` → `bash` → `sh`.
pub(super) fn resolve_shell_program(dirs: &Dirs) -> anyhow::Result<ShellProgram> {
    if let Ok(p) = std::env::var("RUSTLING_TULIP_SHELL") {
        let label = shell_label_from_path(&p);
        let (args, extra_env) = plain_shell_args(dirs, &label)?;
        return Ok(ShellProgram {
            program: p,
            args,
            label,
            extra_env,
        });
    }
    #[cfg(windows)]
    {
        for (candidate, label) in [
            ("pwsh.exe", "pwsh"),
            ("powershell.exe", "powershell"),
            ("cmd.exe", "cmd"),
        ] {
            if which::which(candidate).is_ok() {
                let label = label.to_string();
                let (args, extra_env) = plain_shell_args(dirs, &label)?;
                return Ok(ShellProgram {
                    program: candidate.to_string(),
                    args,
                    label,
                    extra_env,
                });
            }
        }
        anyhow::bail!("no shell on PATH (tried pwsh.exe, powershell.exe, cmd.exe)")
    }
    #[cfg(not(windows))]
    {
        if let Ok(p) = std::env::var("SHELL") {
            let label = shell_label_from_path(&p);
            let (args, extra_env) = plain_shell_args(dirs, &label)?;
            return Ok(ShellProgram {
                program: p,
                args,
                label,
                extra_env,
            });
        }
        for (candidate, label) in [("bash", "bash"), ("zsh", "zsh"), ("sh", "sh")] {
            if which::which(candidate).is_ok() {
                let label = label.to_string();
                let (args, extra_env) = plain_shell_args(dirs, &label)?;
                return Ok(ShellProgram {
                    program: candidate.to_string(),
                    args,
                    label,
                    extra_env,
                });
            }
        }
        anyhow::bail!("no shell on PATH (tried bash, zsh, sh)")
    }
}

/// The ceiling of the wait for the shell's prompt before the agent's command
/// line is typed; the injector types it once the prompt has printed.
const SHELL_LAUNCH_DELAY_MS: u32 = 2000;

/// The pause between clearing the shell's input line and typing the command,
/// so a pseudo console does not read the clearing Esc and the first typed
/// character as one Alt chord.
const CLEAR_LINE_PAUSE_MS: u32 = 100;

/// The ceiling of the wait for the agent's TUI before the initial prompt is
/// typed; the injector types it once the TUI has painted.
const AGENT_PROMPT_DELAY_MS: u32 = 15_000;

/// The command-line syntax of the shell an agent's command line is typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellFamily {
    PowerShell,
    Cmd,
    Posix,
}

impl ShellFamily {
    /// The family of the shell `program` runs, from its file stem: `pwsh` and
    /// `powershell` are PowerShell, `cmd` is cmd, and `sh`, `bash`, `zsh`,
    /// `dash` and `ksh` are POSIX sh. `None` for any other shell, and for a
    /// POSIX shell that Windows runs through WSL (under `System32` or
    /// `WindowsApps`), which takes Linux paths.
    pub(super) fn of(program: &str) -> Option<Self> {
        match program_stem(program).as_str() {
            "pwsh" | "powershell" => Some(Self::PowerShell),
            "cmd" => Some(Self::Cmd),
            "sh" | "bash" | "zsh" | "dash" | "ksh" if !runs_through_wsl(program) => {
                Some(Self::Posix)
            }
            _ => None,
        }
    }

    /// The bytes that clear the shell's input line: Esc for PowerShell and
    /// cmd, Ctrl+U for a POSIX shell's line editor.
    fn clear_line(self) -> &'static [u8] {
        match self {
            Self::PowerShell | Self::Cmd => b"\x1b",
            Self::Posix => b"\x15",
        }
    }
}

/// `program`'s file stem, lower-cased; empty when it has none.
fn program_stem(program: &str) -> String {
    Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// Whether `program` is a Windows path into the system or app-alias folders,
/// where a POSIX shell is WSL's.
fn runs_through_wsl(program: &str) -> bool {
    let lower = program.to_ascii_lowercase();
    lower.contains(r"\system32\") || lower.contains(r"\windowsapps\")
}

/// Whether the agent's arguments pass through a parser that mangles some of
/// them: Windows PowerShell 5.1 hosting the agent, or cmd running it (the
/// resolved agent program is `cmd.exe` or a batch file). `agent` is `None`
/// for a program word typed verbatim, which is not a path to inspect.
fn legacy_argument_passing(shell_program: &str, agent: Option<&str>) -> bool {
    let agent = agent.unwrap_or_default();
    let ext = Path::new(agent)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    program_stem(shell_program) == "powershell"
        || program_stem(agent) == "cmd"
        || matches!(ext.as_deref(), Some("cmd" | "bat"))
}

/// An argument a legacy parser mangles: empty, holding a double quote, or
/// ending in a backslash.
fn mangled_by_legacy_parsing(arg: &str) -> bool {
    arg.is_empty() || arg.contains('"') || arg.ends_with('\\')
}

/// `argv`, the program first, as one line `family` runs. `None` when cmd
/// cannot carry an argument: one holding a double quote, a line break, `%`
/// or `!`.
pub(super) fn quote_command_line(family: ShellFamily, argv: &[String]) -> Option<String> {
    let quoted = quote_args(family, argv)?;
    match family {
        ShellFamily::PowerShell => Some(format!("& {quoted}")),
        ShellFamily::Cmd | ShellFamily::Posix => Some(quoted),
    }
}

/// `args`, each quoted for `family`, joined by spaces. `None` when cmd cannot
/// carry one (see [`cmd_refuses`]).
fn quote_args(family: ShellFamily, args: &[String]) -> Option<String> {
    let quoted: Option<Vec<String>> = match family {
        ShellFamily::PowerShell => Some(args.iter().map(|arg| powershell_quote(arg)).collect()),
        ShellFamily::Cmd => args.iter().map(|arg| cmd_quote(arg)).collect(),
        ShellFamily::Posix => Some(args.iter().map(|arg| posix_quote(arg)).collect()),
    };
    Some(quoted?.join(" "))
}

/// The agent's command as `family` types it: `argv` quoted (see
/// [`quote_command_line`]), or the user's program word bare followed by
/// `argv` quoted, then the user's extra arguments verbatim. `None` when cmd
/// cannot carry an argument, or the extras hold `%` or `!`, which cmd expands.
fn typed_command(family: ShellFamily, argv: &[String], typed: TypedCommand<'_>) -> Option<String> {
    let extra = typed.extra.trim();
    if matches!(family, ShellFamily::Cmd) && extra.contains(['%', '!']) {
        return None;
    }
    let mut line = match typed.program_word {
        None => quote_command_line(family, argv)?,
        Some(word) if argv.is_empty() => word.to_owned(),
        Some(word) => format!("{word} {}", quote_args(family, argv)?),
    };
    if !extra.is_empty() {
        line.push(' ');
        line.push_str(extra);
    }
    Some(line)
}

/// The line that runs `command` from `cwd` in `family`: a change to `cwd`,
/// then the command. `None` when cmd cannot quote the folder.
fn launch_line(family: ShellFamily, cwd: &str, command: &str) -> Option<String> {
    match family {
        ShellFamily::PowerShell => Some(format!(
            "Set-Location -LiteralPath {}; {command}",
            powershell_quote(cwd)
        )),
        ShellFamily::Cmd => {
            if cmd_refuses(cwd) {
                return None;
            }
            Some(format!("cd /d \"{cwd}\" && {command}"))
        }
        ShellFamily::Posix => Some(format!("cd -- {} && {command}", posix_quote(cwd))),
    }
}

/// A POSIX sh argument: single-quoted, each single quote closed, escaped and
/// reopened.
fn posix_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// A PowerShell argument: single-quoted, each single quote (typographic ones
/// included, which PowerShell also reads as quotes) doubled. An argument
/// holding a line break is double-quoted instead, its breaks written as
/// backtick escapes so the typed command stays one line.
fn powershell_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    if arg.contains(['\n', '\r']) {
        out.push('"');
        for ch in arg.chars() {
            match ch {
                '\n' => out.push_str("`n"),
                '\r' => out.push_str("`r"),
                '`' | '"' | '$' | '\u{201C}' | '\u{201D}' | '\u{201E}' => {
                    out.push('`');
                    out.push(ch);
                }
                _ => out.push(ch),
            }
        }
        out.push('"');
    } else {
        out.push('\'');
        for ch in arg.chars() {
            if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
                out.push(ch);
            }
            out.push(ch);
        }
        out.push('\'');
    }
    out
}

/// Whether cmd cannot carry `arg` in a typed line: a double quote or a line
/// break ends it, and `%` and `!` expand even inside quotes.
fn cmd_refuses(arg: &str) -> bool {
    arg.contains(['"', '\n', '\r', '%', '!'])
}

/// A cmd argument: double-quoted when empty or holding a space or a shell
/// metacharacter, its trailing backslashes doubled so the closing quote stays
/// a quote; `None` for one cmd cannot carry (see [`cmd_refuses`]).
fn cmd_quote(arg: &str) -> Option<String> {
    if cmd_refuses(arg) {
        return None;
    }
    let special = |ch: char| ch.is_whitespace() || "&|<>^(),;=".contains(ch);
    if !arg.is_empty() && !arg.contains(special) {
        return Some(arg.to_owned());
    }
    let trailing = arg.len() - arg.trim_end_matches('\\').len();
    Some(format!("\"{arg}{}\"", "\\".repeat(trailing)))
}

/// How an agent session's PTY child starts.
pub(super) enum Launch {
    /// The agent is the PTY child.
    Direct,
    /// The shell is the PTY child and the agent is typed into it.
    InShell(ShellLaunch),
}

/// A shell that hosts the agent: the shell the PTY runs, and the injectors,
/// run one after another, that type the agent's command line into it and
/// then the initial prompt (or the preset's script) into the agent.
pub(super) struct ShellLaunch {
    pub(super) shell: ShellProgram,
    pub(super) injectors: Vec<protocol::PromptInjector>,
}

/// The agent a launch starts: its program and arguments without the initial
/// prompt (`argv`; the arguments alone when `typed` carries a program word),
/// what the user's launch command adds, the folder it starts in, the prompt,
/// and a preset's script, which carries the prompt itself.
pub(super) struct AgentLaunch<'a> {
    pub(super) argv: &'a [String],
    pub(super) typed: TypedCommand<'a>,
    pub(super) cwd: &'a str,
    pub(super) initial_prompt: Option<&'a str>,
    pub(super) preset: Option<protocol::PromptInjector>,
}

/// What the user's launch command adds to a typed agent line.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TypedCommand<'a> {
    /// The program word, typed bare in place of the resolved program so an
    /// alias or function from the shell's profile resolves; `None` types the
    /// resolved program, quoted, as `argv`'s first element.
    pub(super) program_word: Option<&'a str>,
    /// Extra arguments typed verbatim after the agent's own; blank for none.
    pub(super) extra: &'a str,
}

/// The program and extra arguments an interactive spawn launches its agent
/// with, chosen by [`choose_agent_command`].
#[derive(Debug)]
pub(super) struct AgentCommandChoice {
    /// The program, before [`resolve_agent_program`]: the agent's environment
    /// variable, else the setting's program, else the plugin's default.
    pub(super) program: String,
    /// Whether the typed shell form types [`Self::program`] verbatim: a
    /// program from the setting that names no existing file.
    pub(super) typed_verbatim: bool,
    /// The extra arguments split by POSIX shell-word rules, for the direct
    /// spawn.
    pub(super) extra_args: Vec<String>,
    /// The extra arguments as stored, trimmed, for the typed shell form.
    pub(super) extra_text: String,
}

impl AgentCommandChoice {
    /// The direct spawn's arguments: the resolver's leading `prepend`, the
    /// plugin's own `plugin_args`, then the extra arguments.
    pub(super) fn direct_args(
        &self,
        prepend: Vec<String>,
        plugin_args: Vec<String>,
    ) -> Vec<String> {
        let mut args = prepend;
        args.extend(plugin_args);
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

/// Choose the program and extra arguments `plugin`'s interactive spawn uses:
/// the plugin's environment variable (read through `env`) wins over the
/// stored `setting`'s program, which wins over the plugin's default. The
/// extra arguments apply whichever program wins.
///
/// # Errors
///
/// The extra arguments do not split (an unclosed quote), which refuses the
/// spawn with an error naming the setting.
pub(super) fn choose_agent_command(
    plugin: &dyn AgentPlugin,
    setting: &AgentCommand,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<AgentCommandChoice> {
    let extra_args = shell_words::split(&setting.args).map_err(|err| {
        anyhow::anyhow!(
            "The {} launch command's extra arguments cannot be split: {err}. Fix them in Settings.",
            plugin.descriptor().label
        )
    })?;
    let set_program = setting.program.trim();
    let (program, typed_verbatim) = if let Some(path) = env(plugin.program_env_var()) {
        (path, false)
    } else if set_program.is_empty() {
        (plugin.default_program().to_owned(), false)
    } else {
        (set_program.to_owned(), !Path::new(set_program).is_file())
    };
    Ok(AgentCommandChoice {
        program,
        typed_verbatim,
        extra_args,
        extra_text: setting.args.trim().to_owned(),
    })
}

/// Plan how an agent whose descriptor says `shell_resume` starts: inside
/// `shell` when there is one, its family is known, and it can carry the
/// agent's folder and arguments as typed, else directly. The initial prompt
/// is typed only without a preset script.
pub(super) fn plan_launch(
    shell_resume: bool,
    shell: Option<ShellProgram>,
    agent: AgentLaunch<'_>,
) -> Launch {
    let Some(shell) = shell.filter(|_| shell_resume) else {
        return Launch::Direct;
    };
    let Some(family) = ShellFamily::of(&shell.program) else {
        tracing::warn!(
            shell = %shell.label,
            "the agent cannot be typed into this shell; spawning the agent directly"
        );
        return Launch::Direct;
    };
    let resolved_program = match agent.typed.program_word {
        Some(_) => None,
        None => agent.argv.first().map(String::as_str),
    };
    let legacy_refused = legacy_argument_passing(&shell.program, resolved_program)
        && agent.argv.iter().any(|arg| mangled_by_legacy_parsing(arg));
    let line = typed_command(family, agent.argv, agent.typed)
        .and_then(|command| launch_line(family, agent.cwd, &command))
        .filter(|_| !legacy_refused);
    let Some(line) = line else {
        tracing::warn!(
            shell = %shell.label,
            "the agent's command line cannot be typed into this shell; spawning the agent directly"
        );
        return Launch::Direct;
    };
    let mut injectors = vec![typed_launch(family, line)];
    if let Some(preset) = agent.preset {
        injectors.push(preset);
    } else if let Some(prompt) = agent.initial_prompt.filter(|prompt| !prompt.is_empty()) {
        injectors.push(typed_prompt(prompt));
    }
    Launch::InShell(ShellLaunch { shell, injectors })
}

/// The injector that types `line` into a `family` shell once its prompt has
/// printed, clearing whatever the input line already holds first.
fn typed_launch(family: ShellFamily, line: String) -> protocol::PromptInjector {
    use base64::Engine as _;
    protocol::PromptInjector {
        steps: vec![
            protocol::InjectorStep::Delay {
                ms: SHELL_LAUNCH_DELAY_MS,
            },
            protocol::InjectorStep::Write {
                data_b64: base64::engine::general_purpose::STANDARD.encode(family.clear_line()),
            },
            protocol::InjectorStep::Delay {
                ms: CLEAR_LINE_PAUSE_MS,
            },
            protocol::InjectorStep::Text {
                content: line,
                newline: true,
            },
        ],
        verify_mode_marker: None,
        startup: protocol::InjectorStartup::ShellPrompt,
    }
}

/// The injector that types `prompt` into the agent's TUI once it has painted.
fn typed_prompt(prompt: &str) -> protocol::PromptInjector {
    protocol::PromptInjector {
        steps: vec![
            protocol::InjectorStep::Delay {
                ms: AGENT_PROMPT_DELAY_MS,
            },
            protocol::InjectorStep::Text {
                content: prompt.to_owned(),
                newline: false,
            },
            protocol::InjectorStep::Text {
                content: String::new(),
                newline: true,
            },
        ],
        verify_mode_marker: None,
        startup: protocol::InjectorStartup::AgentTui,
    }
}

fn shell_label_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| "shell".to_string(), str::to_lowercase)
}

/// True iff shell integration (OSC 133 + OSC 633 hooks) should be injected.
/// Defaulted on; set `RUSTLING_TULIP_SHELL_INTEGRATION=0` to disable for
/// debugging or when the user ships their own integration in `.bashrc` etc.
fn shell_integration_enabled() -> bool {
    match std::env::var("RUSTLING_TULIP_SHELL_INTEGRATION") {
        Ok(v) => !matches!(v.trim(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

type ShellLaunchExtras = (Vec<String>, Vec<(String, String)>);

fn plain_shell_args(dirs: &Dirs, label: &str) -> anyhow::Result<ShellLaunchExtras> {
    let integrate = shell_integration_enabled();
    match label {
        "pwsh" | "powershell" => {
            let script = if integrate {
                powershell_prompt_script_with_integration()
            } else {
                powershell_cwd_prompt_script_minimal()
            };
            Ok((
                vec!["-NoExit".to_string(), "-Command".to_string(), script],
                Vec::new(),
            ))
        }
        "cmd" => Ok((
            vec![
                "/K".to_string(),
                "prompt $E]7;file:///$P$E\\$P$G".to_string(),
            ],
            Vec::new(),
        )),
        "bash" => {
            if !integrate {
                return Ok((Vec::new(), Vec::new()));
            }
            let path = ensure_bash_rcfile(dirs)?;
            Ok((
                vec![
                    "--rcfile".to_string(),
                    path.to_string_lossy().into_owned(),
                    "-i".to_string(),
                ],
                Vec::new(),
            ))
        }
        "zsh" => {
            if !integrate {
                return Ok((Vec::new(), Vec::new()));
            }
            let zdotdir = ensure_zsh_dotdir(dirs)?;
            let mut env = vec![(
                "ZDOTDIR".to_string(),
                zdotdir.to_string_lossy().into_owned(),
            )];
            // Preserve the user's original ZDOTDIR so our .zshrc can source
            // the right rc file. Falls back to $HOME inside the script.
            if let Ok(orig) = std::env::var("ZDOTDIR") {
                env.push(("RUSTLING_TULIP_ZSH_USER_ZDOTDIR".to_string(), orig));
            }
            Ok((Vec::new(), env))
        }
        _ => Ok((Vec::new(), Vec::new())),
    }
}

/// Directory used to stage shell-integration init scripts (bash rcfile,
/// zsh ZDOTDIR). Lives under the config root so it survives reboots; the
/// content is regenerated on demand whenever the daemon writes a script.
fn shell_init_dir(dirs: &Dirs) -> PathBuf {
    dirs.config.join("shell-init")
}

fn ensure_bash_rcfile(dirs: &Dirs) -> anyhow::Result<PathBuf> {
    let dir = shell_init_dir(dirs);
    std::fs::create_dir_all(&dir).context("creating shell-init dir")?;
    let path = dir.join("bash-init.sh");
    std::fs::write(&path, BASH_INIT_SCRIPT).context("writing bash-init.sh")?;
    Ok(path)
}

fn ensure_zsh_dotdir(dirs: &Dirs) -> anyhow::Result<PathBuf> {
    let dir = shell_init_dir(dirs).join("zsh-dotdir");
    std::fs::create_dir_all(&dir).context("creating zsh ZDOTDIR")?;
    let path = dir.join(".zshrc");
    std::fs::write(&path, ZSH_INIT_SCRIPT).context("writing zsh .zshrc")?;
    Ok(dir)
}

/// PowerShell prompt-wrapper script — emits OSC 7 (cwd), plus OSC 133;A/B/D
/// and OSC 633;E for the frontend's per-command chip UI. The previous
/// command's exit code is read from `$LASTEXITCODE` / `$?` inside the
/// prompt function, which PowerShell calls between commands. OSC 133;C and
/// OSC 633;E are emitted from a `PSReadLine` `AddToHistoryHandler` so
/// they land the instant the user presses Enter, before the command starts
/// running. If `PSReadLine` isn't loaded the chip still works (D + A marks
/// only); the frontend falls back to scraping the command text out of the
/// terminal buffer.
fn powershell_prompt_script_with_integration() -> String {
    // Architecture follows VSCode's shellIntegration.ps1 (MIT):
    // - Wrap `PSConsoleHostReadLine` (PSReadLine's host hook) so every
    //   committed line emits OSC 633;E + OSC 133;C and flips a flag.
    //   This fires for ALL commands, including duplicates that
    //   `AddToHistoryHandler` skips when `HistoryNoDuplicates=$true`.
    // - In `prompt`, if the flag was set, emit OSC 133;D with `[int]!$?`
    //   as the exit code. Sidesteps `$LASTEXITCODE` sticky behavior
    //   entirely by using only PowerShell's last-statement success bit.
    // - Re-emit a soft error before calling the original prompt so its
    //   own `$?` check still reflects the user's command outcome.
    [
        // Capture or stub the original prompt function.
        "$global:__rt_original_prompt = if (Test-Path function:\\prompt) {",
        "(Get-Command prompt).ScriptBlock",
        "} else {",
        "{ \"PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) \" }",
        "};",
        "$global:__rt_in_command = $false;",
        "$global:__rt_orig_psreadline = $null;",
        // Escape per VSCode's OSC 633 spec: \\, semicolons, newlines.
        "function global:__rt_esc {",
        "param([string]$s)",
        "if ($null -eq $s) { return '' };",
        "$s = $s -replace '\\\\', '\\\\\\\\';",
        "$s = $s -replace ';', '\\x3b';",
        "$s = $s -replace \"`n\", '\\x0a';",
        "$s",
        "};",
        // OSC 7 cwd emitter — kept for cwd tracking outside shell integration.
        "function global:__rt_emit_cwd {",
        "try {",
        "$path = (Get-Location).ProviderPath;",
        "if ($path) {",
        "$uri = [System.Uri]::new($path).AbsoluteUri;",
        "[Console]::Out.Write([char]27 + ']7;' + $uri + [char]7)",
        "}",
        "} catch { Write-Debug $_ };",
        "};",
        // Prompt function: emit D for the just-finished command (when
        // the PSConsoleHostReadLine wrapper set the in-command flag),
        // then A + the original prompt text + B. Returns one composed
        // string so PowerShell prints the marks in the right visual
        // position relative to the visible prompt.
        "function global:prompt {",
        "$rt_exit = $null;",
        "if ($global:__rt_in_command) {",
        // [int]!$? is 0 for success, 1 for failure. Simple and not
        // poisoned by $LASTEXITCODE staleness; matches VSCode's
        // approach. Real native-process exit codes aren't reported,
        // but a binary success/failure signal is enough for the chip.
        "$rt_exit = [int]!$global:?;",
        "$global:__rt_in_command = $false;",
        // Re-set $? to its pre-prompt value so the user's own prompt
        // function sees the correct success state for the last
        // command. Writing an ignored soft error flips $? to false.
        "if ($rt_exit -ne 0) { Write-Error 'command failed' -ErrorAction Ignore };",
        "};",
        "global:__rt_emit_cwd;",
        "$esc = [char]27; $bel = [char]7;",
        "$marks = '';",
        "if ($null -ne $rt_exit) { $marks += \"$esc]133;D;$rt_exit$bel\" };",
        "$marks += \"$esc]133;A$bel\";",
        "$orig = try { & $global:__rt_original_prompt } catch { 'PS> ' };",
        "\"$marks$orig$esc]133;B$bel\"",
        "};",
        // Wrap PSConsoleHostReadLine — PSReadLine's host entry point
        // for line editing. The wrapper fires for every committed line
        // including duplicates that AddToHistoryHandler skips when
        // HistoryNoDuplicates=$true. Empty/whitespace input is filtered
        // so a bare Enter doesn't produce a phantom chip.
        "try {",
        "if (Get-Module -ListAvailable PSReadLine) {",
        "Import-Module PSReadLine -ErrorAction SilentlyContinue",
        "};",
        "if (Get-Command PSConsoleHostReadLine -ErrorAction SilentlyContinue) {",
        "$global:__rt_orig_psreadline = $function:PSConsoleHostReadLine;",
        "function global:PSConsoleHostReadLine {",
        "$cmd = $global:__rt_orig_psreadline.Invoke();",
        "if (-not [string]::IsNullOrWhiteSpace($cmd)) {",
        "$esc = [char]27; $bel = [char]7;",
        "$escaped = global:__rt_esc $cmd;",
        "[Console]::Out.Write(\"$esc]633;E;$escaped$bel\");",
        "[Console]::Out.Write(\"$esc]133;C$bel\");",
        "$global:__rt_in_command = $true;",
        "};",
        "$cmd",
        "}",
        "}",
        "} catch { Write-Debug $_ };",
        "global:__rt_emit_cwd",
    ]
    .join(" ")
}

/// PowerShell prompt wrapper without OSC 133/633 — used when
/// `RUSTLING_TULIP_SHELL_INTEGRATION=0`. Keeps the OSC 7 cwd emitter so the
/// session sidebar still tracks `cd` changes.
fn powershell_cwd_prompt_script_minimal() -> String {
    [
        "$global:__rt_original_prompt = if (Test-Path function:\\prompt) {",
        "(Get-Command prompt).ScriptBlock",
        "} else {",
        "{ \"PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) \" }",
        "};",
        "function global:__rt_emit_cwd {",
        "try {",
        "$path = (Get-Location).ProviderPath;",
        "if ($path) {",
        "$uri = [System.Uri]::new($path).AbsoluteUri;",
        "[Console]::Out.Write([char]27 + ']7;' + $uri + [char]7)",
        "}",
        "} catch { Write-Debug $_ };",
        "}",
        "function global:prompt {",
        "global:__rt_emit_cwd;",
        "& $global:__rt_original_prompt",
        "};",
        "global:__rt_emit_cwd",
    ]
    .join(" ")
}

const BASH_INIT_SCRIPT: &str = include_str!("../../../shell-init/bash-init.sh");

const ZSH_INIT_SCRIPT: &str = include_str!("../../../shell-init/zsh-init.zshrc");

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::expect_used,
    reason = "tests fail on an unexpected launch shape with panic or expect; failure messages aid debugging"
)]
mod launch_tests {
    use super::{
        AgentLaunch, Launch, ShellFamily, ShellProgram, TypedCommand, plan_launch,
        quote_command_line,
    };
    use base64::Engine as _;
    use protocol::{InjectorStartup, InjectorStep, PromptInjector};

    const CWD: &str = r"C:\work\repo";

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn shell(program: &str, label: &str) -> ShellProgram {
        ShellProgram {
            program: program.to_owned(),
            args: strings(&["-NoExit"]),
            label: label.to_owned(),
            extra_env: vec![("RT_SHELL".to_owned(), "1".to_owned())],
        }
    }

    fn agent<'a>(argv: &'a [String], prompt: Option<&'a str>) -> AgentLaunch<'a> {
        AgentLaunch {
            argv,
            typed: TypedCommand::default(),
            cwd: CWD,
            initial_prompt: prompt,
            preset: None,
        }
    }

    /// The typed line of the launch `shell` plans for `argv` with the user's
    /// launch command `typed`, or `None` for the direct spawn.
    fn typed_line_with(
        shell: ShellProgram,
        argv: &[String],
        typed: TypedCommand<'_>,
    ) -> Option<String> {
        let launch = AgentLaunch {
            typed,
            ..agent(argv, None)
        };
        match plan_launch(true, Some(shell), launch) {
            Launch::Direct => None,
            Launch::InShell(launch) => launch
                .injectors
                .first()
                .and_then(|typed| texts(typed).first().map(|(line, _)| (*line).to_owned())),
        }
    }

    #[test]
    fn a_launch_command_types_its_program_word_bare_and_its_extras_verbatim() {
        let plugin_args = strings(&["--model", "opus", "it's"]);
        let typed = TypedCommand {
            program_word: Some("cl"),
            extra: r#" --debug "a b" "#,
        };
        assert_eq!(
            typed_line_with(shell("pwsh.exe", "pwsh"), &plugin_args, typed).as_deref(),
            Some(
                r#"Set-Location -LiteralPath 'C:\work\repo'; cl '--model' 'opus' 'it''s' --debug "a b""#
            ),
            "PowerShell types the word without the call operator, so an alias or function resolves"
        );
        assert_eq!(
            typed_line_with(shell("cmd.exe", "cmd"), &plugin_args, typed).as_deref(),
            Some(r#"cd /d "C:\work\repo" && cl --model opus it's --debug "a b""#),
            "cmd types a double quote in the extras as it is"
        );
        assert_eq!(
            typed_line_with(shell("/bin/bash", "bash"), &plugin_args, typed).as_deref(),
            Some(r#"cd -- 'C:\work\repo' && cl '--model' 'opus' 'it'\''s' --debug "a b""#),
            "bash expands an alias only from an unquoted first word"
        );
        assert_eq!(
            typed_line_with(
                shell("powershell.exe", "powershell"),
                &strings(&["--model", "opus"]),
                typed
            )
            .as_deref(),
            Some(r#"Set-Location -LiteralPath 'C:\work\repo'; cl '--model' 'opus' --debug "a b""#),
            "the legacy check reads the plugin args, not the verbatim word or the extras"
        );
    }

    #[test]
    fn a_blank_launch_command_keeps_the_fully_quoted_line() {
        let argv = strings(&[r"C:\bin\claude.exe", "--model", "opus"]);
        for blank in ["", "   "] {
            let typed = TypedCommand {
                program_word: None,
                extra: blank,
            };
            assert_eq!(
                typed_line_with(shell("pwsh.exe", "pwsh"), &argv, typed).as_deref(),
                Some(
                    r"Set-Location -LiteralPath 'C:\work\repo'; & 'C:\bin\claude.exe' '--model' 'opus'"
                )
            );
            assert_eq!(
                typed_line_with(shell("cmd.exe", "cmd"), &argv, typed).as_deref(),
                Some(r#"cd /d "C:\work\repo" && C:\bin\claude.exe --model opus"#)
            );
            assert_eq!(
                typed_line_with(shell("/bin/bash", "bash"), &argv, typed).as_deref(),
                Some(r"cd -- 'C:\work\repo' && 'C:\bin\claude.exe' '--model' 'opus'")
            );
        }
    }

    #[test]
    fn a_resolved_launch_program_stays_quoted_with_the_extras_after() {
        // A program from RUSTLING_TULIP_CLAUDE, or a setting naming an
        // existing file, is resolved to a path and quoted like the default.
        let argv = strings(&[r"C:\my tools\claude.exe", "--model", "opus"]);
        let typed = TypedCommand {
            program_word: None,
            extra: "--debug",
        };
        assert_eq!(
            typed_line_with(shell("pwsh.exe", "pwsh"), &argv, typed).as_deref(),
            Some(
                r"Set-Location -LiteralPath 'C:\work\repo'; & 'C:\my tools\claude.exe' '--model' 'opus' --debug"
            )
        );
        assert_eq!(
            typed_line_with(shell("cmd.exe", "cmd"), &argv, typed).as_deref(),
            Some(r#"cd /d "C:\work\repo" && "C:\my tools\claude.exe" --model opus --debug"#)
        );
        assert_eq!(
            typed_line_with(shell("/bin/bash", "bash"), &argv, typed).as_deref(),
            Some(r"cd -- 'C:\work\repo' && 'C:\my tools\claude.exe' '--model' 'opus' --debug")
        );
    }

    #[test]
    fn cmd_falls_back_to_the_direct_launch_on_percent_or_bang_in_the_extras() {
        let plugin_args = strings(&["--model", "opus"]);
        for extra in ["--name %USERNAME%", "--say hi!"] {
            for program_word in [Some("cl"), None] {
                let argv = if program_word.is_some() {
                    plugin_args.clone()
                } else {
                    strings(&[r"C:\bin\claude.exe", "--model", "opus"])
                };
                let typed = TypedCommand {
                    program_word,
                    extra,
                };
                assert!(
                    typed_line_with(shell("cmd.exe", "cmd"), &argv, typed).is_none(),
                    "cmd would expand {extra:?}"
                );
                assert!(
                    typed_line_with(shell("pwsh.exe", "pwsh"), &argv, typed).is_some(),
                    "PowerShell types {extra:?} as it is"
                );
            }
        }
    }

    fn texts(injector: &PromptInjector) -> Vec<(&str, bool)> {
        injector
            .steps
            .iter()
            .filter_map(|step| match step {
                InjectorStep::Text { content, newline } => Some((content.as_str(), *newline)),
                _ => None,
            })
            .collect()
    }

    /// The bytes the launch's `Write` steps send before its text.
    fn writes(injector: &PromptInjector) -> Vec<Vec<u8>> {
        injector
            .steps
            .iter()
            .filter_map(|step| match step {
                InjectorStep::Write { data_b64 } => base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .ok(),
                _ => None,
            })
            .collect()
    }

    /// The typed line of the launch `shell` plans for `argv`, or `None` for
    /// the direct spawn.
    fn typed_line(shell: ShellProgram, argv: &[String]) -> Option<String> {
        match plan_launch(true, Some(shell), agent(argv, None)) {
            Launch::Direct => None,
            Launch::InShell(launch) => launch
                .injectors
                .first()
                .and_then(|typed| texts(typed).first().map(|(line, _)| (*line).to_owned())),
        }
    }

    #[test]
    fn powershell_quotes_with_the_call_operator_and_doubled_single_quotes() {
        let argv = strings(&[r"C:\Program Files\claude.exe", "--model", "it's", ""]);
        assert_eq!(
            quote_command_line(ShellFamily::PowerShell, &argv).as_deref(),
            Some(r"& 'C:\Program Files\claude.exe' '--model' 'it''s' ''")
        );
        assert_eq!(
            quote_command_line(ShellFamily::PowerShell, &strings(&["a\u{2019}b"])).as_deref(),
            Some("& 'a\u{2019}\u{2019}b'"),
            "a typographic single quote also ends a single-quoted string, so it is doubled too"
        );
        assert_eq!(
            quote_command_line(
                ShellFamily::PowerShell,
                &strings(&["x", "one\ntwo $HOME `\""])
            )
            .as_deref(),
            Some("& 'x' \"one`ntwo `$HOME ```\"\""),
            "a multi-line argument is one double-quoted line with backtick escapes"
        );
        assert_eq!(
            ShellFamily::of(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Some(ShellFamily::PowerShell)
        );
        assert_eq!(
            ShellFamily::of("powershell.exe"),
            Some(ShellFamily::PowerShell)
        );
    }

    #[test]
    fn cmd_quotes_spaces_and_refuses_a_double_quote() {
        let argv = strings(&[
            r"C:\bin\claude.exe",
            "--add-dir",
            r"C:\my dir\",
            "a&b",
            "plain",
            "",
        ]);
        assert_eq!(
            quote_command_line(ShellFamily::Cmd, &argv).as_deref(),
            Some(r#"C:\bin\claude.exe --add-dir "C:\my dir\\" "a&b" plain """#)
        );
        assert_eq!(
            quote_command_line(ShellFamily::Cmd, &strings(&["claude", r#"say "hi""#])),
            None
        );
        assert_eq!(
            quote_command_line(ShellFamily::Cmd, &strings(&["claude", "one\ntwo"])),
            None,
            "cmd ends a typed command at a newline"
        );
        assert_eq!(ShellFamily::of("cmd.exe"), Some(ShellFamily::Cmd));
    }

    #[test]
    fn cmd_refuses_percent_and_bang() {
        for arg in ["%PATH%", "50%", "hi!", "!VAR!"] {
            assert_eq!(
                quote_command_line(ShellFamily::Cmd, &strings(&["claude", arg])),
                None,
                "cmd expands {arg:?} even inside quotes"
            );
        }
        assert!(
            typed_line(
                shell("cmd.exe", "cmd"),
                &strings(&["claude", "--model", "50%"])
            )
            .is_none(),
            "an argument cmd would expand keeps the direct spawn"
        );
    }

    #[test]
    fn posix_quotes_with_single_quotes() {
        let argv = strings(&["/usr/bin/claude", "--model", "it's", "$HOME"]);
        assert_eq!(
            quote_command_line(ShellFamily::Posix, &argv).as_deref(),
            Some(r"'/usr/bin/claude' '--model' 'it'\''s' '$HOME'")
        );
        for known in ["/bin/bash", "zsh", "/bin/sh", "dash", "ksh"] {
            assert_eq!(ShellFamily::of(known), Some(ShellFamily::Posix), "{known}");
        }
    }

    #[test]
    fn an_unknown_shell_keeps_the_direct_spawn() {
        for unknown in [
            "nu",
            "/usr/bin/fish",
            "tcsh",
            r"C:\Windows\System32\bash.exe",
            r"C:\Windows\System32\wsl.exe",
        ] {
            assert_eq!(ShellFamily::of(unknown), None, "{unknown}");
            assert!(
                typed_line(shell(unknown, "shell"), &strings(&["claude"])).is_none(),
                "{unknown} keeps the direct spawn"
            );
        }
    }

    #[test]
    fn legacy_powershell_falls_back_on_empty_quote_or_trailing_backslash() {
        for arg in ["", r#"say "hi""#, r"C:\dir\"] {
            let argv = strings(&["claude", "--add-dir", arg]);
            assert!(
                typed_line(shell("powershell.exe", "powershell"), &argv).is_none(),
                "Windows PowerShell mangles {arg:?}"
            );
            let through_cmd = strings(&["cmd.exe", "/d", "/c", r"C:\bin\claude.cmd", arg]);
            assert!(
                typed_line(shell("pwsh.exe", "pwsh"), &through_cmd).is_none(),
                "cmd.exe running the agent mangles {arg:?}"
            );
            let shim = strings(&[r"C:\bin\claude.cmd", arg]);
            assert!(
                typed_line(shell("pwsh.exe", "pwsh"), &shim).is_none(),
                "a .cmd agent mangles {arg:?}"
            );
            assert!(
                typed_line(shell("pwsh.exe", "pwsh"), &argv).is_some(),
                "PowerShell 7 passes {arg:?} as it is"
            );
        }
    }

    #[test]
    fn the_launch_changes_to_the_start_folder_first() {
        let argv = strings(&[r"C:\bin\claude.exe", "--model", "opus"]);
        assert_eq!(
            typed_line(shell("pwsh.exe", "pwsh"), &argv).as_deref(),
            Some(
                r"Set-Location -LiteralPath 'C:\work\repo'; & 'C:\bin\claude.exe' '--model' 'opus'"
            )
        );
        assert_eq!(
            typed_line(shell("cmd.exe", "cmd"), &argv).as_deref(),
            Some(r#"cd /d "C:\work\repo" && C:\bin\claude.exe --model opus"#)
        );
        assert_eq!(
            typed_line(shell("/bin/bash", "bash"), &argv).as_deref(),
            Some(r"cd -- 'C:\work\repo' && 'C:\bin\claude.exe' '--model' 'opus'")
        );
        let launch = AgentLaunch {
            argv: &argv,
            typed: TypedCommand::default(),
            cwd: r"C:\50% off",
            initial_prompt: None,
            preset: None,
        };
        assert!(
            matches!(
                plan_launch(true, Some(shell("cmd.exe", "cmd")), launch),
                Launch::Direct
            ),
            "a folder cmd cannot quote keeps the direct spawn"
        );
    }

    #[test]
    fn the_launch_clears_the_line_first() {
        let argv = strings(&["claude"]);
        for (program, label, clear) in [
            ("pwsh.exe", "pwsh", vec![0x1b_u8]),
            ("cmd.exe", "cmd", vec![0x1b]),
            ("/bin/bash", "bash", vec![0x15]),
        ] {
            let Launch::InShell(launch) =
                plan_launch(true, Some(shell(program, label)), agent(&argv, None))
            else {
                panic!("{label} hosts the agent");
            };
            let typed = launch.injectors.first().expect("the launch injector");
            assert_eq!(writes(typed), vec![clear], "{label} clears its input line");
            let clear_at = typed
                .steps
                .iter()
                .position(|step| matches!(step, InjectorStep::Write { .. }));
            let text_at = typed
                .steps
                .iter()
                .position(|step| matches!(step, InjectorStep::Text { .. }));
            assert!(clear_at < text_at, "{label} clears before it types");
        }
    }

    #[test]
    fn a_shell_resume_agent_spawns_the_shell_and_types_its_command_line() {
        let argv = strings(&[
            r"C:\bin\claude.exe",
            "--settings",
            r"C:\cfg dir\hook-settings.json",
        ]);
        let Launch::InShell(launch) = plan_launch(
            true,
            Some(shell("pwsh.exe", "pwsh")),
            agent(&argv, Some("fix the bug")),
        ) else {
            panic!("a shell-resume agent spawns its shell");
        };
        assert_eq!(
            launch.shell.program, "pwsh.exe",
            "the PTY child is the shell"
        );
        assert_eq!(launch.shell.args, strings(&["-NoExit"]));
        let [typed, prompt] = launch.injectors.as_slice() else {
            panic!("two injectors: the command line, then the prompt");
        };
        assert_eq!(typed.startup, InjectorStartup::ShellPrompt);
        assert!(matches!(
            typed.steps.first(),
            Some(InjectorStep::Delay { .. })
        ));
        assert_eq!(
            texts(typed),
            vec![(
                r"Set-Location -LiteralPath 'C:\work\repo'; & 'C:\bin\claude.exe' '--settings' 'C:\cfg dir\hook-settings.json'",
                true
            )]
        );
        assert_eq!(prompt.startup, InjectorStartup::AgentTui);
        assert!(matches!(
            prompt.steps.first(),
            Some(InjectorStep::Delay { .. })
        ));
        assert_eq!(texts(prompt), vec![("fix the bug", false), ("", true)]);

        let preset = PromptInjector {
            steps: vec![InjectorStep::Text {
                content: "from the preset".to_owned(),
                newline: true,
            }],
            verify_mode_marker: None,
            startup: InjectorStartup::AgentTui,
        };
        let Launch::InShell(launch) = plan_launch(
            true,
            Some(shell("bash", "bash")),
            AgentLaunch {
                argv: &argv,
                typed: TypedCommand::default(),
                cwd: CWD,
                initial_prompt: Some("fix the bug"),
                preset: Some(preset),
            },
        ) else {
            panic!("a shell-resume agent spawns its shell");
        };
        let [typed, preset] = launch.injectors.as_slice() else {
            panic!("a preset's script carries the prompt, after the command line");
        };
        assert_eq!(
            texts(typed),
            vec![(
                r"cd -- 'C:\work\repo' && 'C:\bin\claude.exe' '--settings' 'C:\cfg dir\hook-settings.json'",
                true
            )]
        );
        assert_eq!(texts(preset), vec![("from the preset", true)]);
    }

    #[test]
    fn an_agent_without_shell_resume_keeps_the_direct_spawn() {
        let argv = strings(&["claude", "--model", "opus"]);
        assert!(matches!(
            plan_launch(false, Some(shell("pwsh.exe", "pwsh")), agent(&argv, None)),
            Launch::Direct
        ));
        assert!(
            matches!(plan_launch(true, None, agent(&argv, None)), Launch::Direct),
            "no shell to type into keeps the direct spawn"
        );
        let unquotable = strings(&["claude", r#"say "hi""#]);
        assert!(
            matches!(
                plan_launch(
                    true,
                    Some(shell("cmd.exe", "cmd")),
                    agent(&unquotable, None)
                ),
                Launch::Direct
            ),
            "a command line cmd cannot quote keeps the direct spawn"
        );
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod launch_command_tests {
    use super::choose_agent_command;
    use crate::state::AgentCommand;
    use protocol::Agent;

    fn command(program: &str, args: &str) -> AgentCommand {
        AgentCommand {
            program: program.to_owned(),
            args: args.to_owned(),
        }
    }

    fn no_env(_name: &str) -> Option<String> {
        None
    }

    fn claude_env(name: &str) -> Option<String> {
        (name == "RUSTLING_TULIP_CLAUDE").then(|| r"C:\fake\claude.exe".to_owned())
    }

    #[test]
    fn launch_command_env_beats_the_setting() {
        let plugin = agent_registry::plugin(Agent::Claude);
        let chosen = choose_agent_command(plugin, &command("cl", "--debug"), &claude_env)
            .expect("the command splits");
        assert_eq!(chosen.program, r"C:\fake\claude.exe");
        assert!(!chosen.typed_verbatim, "the variable's path is quoted");
        assert_eq!(
            chosen.extra_args,
            ["--debug"],
            "the variable locks only the program"
        );
        assert_eq!(chosen.extra_text, "--debug");
    }

    #[test]
    fn launch_command_setting_beats_the_default() {
        let plugin = agent_registry::plugin(Agent::Claude);
        let chosen = choose_agent_command(plugin, &command("  cl  ", ""), &no_env)
            .expect("the command splits");
        assert_eq!(chosen.program, "cl");
        assert!(chosen.typed_verbatim, "a name is typed as it is");
        assert_eq!(chosen.extra_args, [] as [&str; 0]);

        let blank = choose_agent_command(plugin, &command(" ", " "), &no_env)
            .expect("a blank command splits");
        assert_eq!(blank.program, "claude", "blank is the plugin's own program");
        assert!(!blank.typed_verbatim);
        assert_eq!(blank.extra_args, [] as [&str; 0]);
        assert_eq!(blank.extra_text, "");
    }

    #[test]
    fn launch_command_naming_an_existing_file_is_not_typed_verbatim() {
        let dir = std::env::temp_dir().join(format!("rt-launch-command-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let file = dir.join("my claude.exe");
        std::fs::write(&file, b"").expect("write the program file");
        let path = file.to_string_lossy().into_owned();

        let plugin = agent_registry::plugin(Agent::Claude);
        let chosen =
            choose_agent_command(plugin, &command(&path, ""), &no_env).expect("the command splits");
        assert_eq!(chosen.program, path);
        assert!(!chosen.typed_verbatim, "a path is not an alias");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_command_extras_follow_the_plugin_argv() {
        let plugin = agent_registry::plugin(Agent::Claude);
        let chosen = choose_agent_command(plugin, &command("", r#"--debug "a b" 'c d'"#), &no_env)
            .expect("the command splits");
        let argv = chosen.direct_args(
            vec!["/d".to_owned(), "/c".to_owned()],
            vec!["--model".to_owned(), "opus".to_owned()],
        );
        assert_eq!(
            argv,
            ["/d", "/c", "--model", "opus", "--debug", "a b", "c d"]
        );
    }

    #[test]
    fn launch_command_split_failure_refuses_the_spawn() {
        let plugin = agent_registry::plugin(Agent::Claude);
        let err = choose_agent_command(plugin, &command("", "--debug \"open"), &no_env)
            .err()
            .map(|err| err.to_string());
        assert_eq!(
            err.as_deref(),
            Some(
                "The Claude launch command's extra arguments cannot be split: missing closing quote. Fix them in Settings."
            )
        );
    }
}

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert preconditions with expect/unwrap; failure messages aid debugging"
)]
mod tests {
    #[cfg(windows)]
    mod npm_shim {
        use super::super::npm_shim_to_native_program;
        use std::fs;
        use std::path::{Path, PathBuf};
        use uuid::Uuid;

        /// RAII scratch dir under the OS temp root (drop removes the tree).
        /// Mirrors the `Scratch` helper in `binary_cache.rs` rather than
        /// pulling in `tempfile` just for tests.
        struct Scratch {
            path: PathBuf,
        }

        impl Scratch {
            fn new(label: &str) -> Self {
                let path = std::env::temp_dir()
                    .join(format!("rt-npm-shim-{label}-{}", Uuid::new_v4().simple()));
                fs::create_dir_all(&path).unwrap();
                Self { path }
            }

            fn path(&self) -> &Path {
                &self.path
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        fn touch(path: &Path) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, b"").unwrap();
        }

        /// Mirrors the `@anthropic-ai/claude-code` native-binary layout
        /// (the package that triggered the original bug — the resolver
        /// fell through to `cmd.exe /d /c claude.cmd` because it only
        /// knew about `bin/<stem>.js`, and the portable-pty + cmd.exe
        /// boundary then swallowed claude's stdout).
        #[test]
        fn scoped_native_exe_is_preferred_over_js() {
            let scratch = Scratch::new("scoped-exe");
            let dir = scratch.path();
            let shim = dir.join("claude.cmd");
            touch(&shim);
            let pkg = dir
                .join("node_modules")
                .join("@anthropic-ai")
                .join("claude-code")
                .join("bin");
            let exe = pkg.join("claude.exe");
            let js = pkg.join("claude.js");
            touch(&exe);
            touch(&js);

            let (program, prepend) =
                npm_shim_to_native_program(&shim).expect("resolver should find the native exe");
            assert_eq!(program, exe);
            assert!(
                prepend.is_empty(),
                "native exe should not need a node prefix, got {prepend:?}"
            );
        }

        /// Older claude installs' layout: no native exe, just the JS
        /// entry. Resolver should fall back to running it under
        /// `node.exe`.
        #[test]
        fn scoped_js_entry_resolved_via_node() {
            let scratch = Scratch::new("scoped-js");
            let dir = scratch.path();
            let shim = dir.join("claude.cmd");
            touch(&shim);
            let js = dir
                .join("node_modules")
                .join("@anthropic-ai")
                .join("claude-code")
                .join("bin")
                .join("claude.js");
            touch(&js);
            // Sibling node.exe so the test doesn't depend on a
            // PATH-resolved `node` being installed on the runner.
            let sibling_node = dir.join("node.exe");
            touch(&sibling_node);

            let (program, prepend) = npm_shim_to_native_program(&shim)
                .expect("resolver should fall back to node + js entry");
            assert_eq!(program, sibling_node);
            assert_eq!(prepend.len(), 1, "expected single js arg, got {prepend:?}");
            assert_eq!(PathBuf::from(&prepend[0]), js);
        }

        #[test]
        fn unscoped_native_exe_is_resolved() {
            let scratch = Scratch::new("unscoped-exe");
            let dir = scratch.path();
            let shim = dir.join("mytool.cmd");
            touch(&shim);
            let exe = dir
                .join("node_modules")
                .join("mytool")
                .join("bin")
                .join("mytool.exe");
            touch(&exe);

            let (program, prepend) = npm_shim_to_native_program(&shim)
                .expect("resolver should find unscoped native exe");
            assert_eq!(program, exe);
            assert_eq!(prepend, [] as [std::string::String; 0]);
        }

        /// A `.cmd` that isn't an npm shim (no sibling `node_modules`)
        /// should return `None` so the caller can fall back to the
        /// `cmd.exe /d /c <shim>` path.
        #[test]
        fn non_npm_shim_returns_none() {
            let scratch = Scratch::new("non-npm");
            let shim = scratch.path().join("randomtool.cmd");
            touch(&shim);
            assert!(npm_shim_to_native_program(&shim).is_none());
        }
    }
}
