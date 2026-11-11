//! Program resolution for spawns: the agent CLI (through npm shims on Windows, and the user's launch command) and the user's shell, with its prompt-integration scripts and init files.

use crate::paths::Dirs;
use crate::spawn_plan::SpawnFailure;
use crate::state::AgentCommand;
use agent_api::AgentPlugin;
use anyhow::Context as _;
use protocol::{DefaultShell, ShellChoice};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Resolve an agent-program name into something `CreateProcess` /
/// portable-pty can actually launch. Returns `(program, prepended_args)`.
///
/// On Windows, agent CLIs distributed via npm (e.g. a scoped package
/// such as `@scope/agent-cli`) land on
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
/// either the native `<stem>.exe` (a native-binary distribution) or
/// `node.exe <stem>.js` (a JS-only package) directly. That removes
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
pub(crate) fn resolve_agent_program(name: &str) -> (String, Vec<String>) {
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
/// - Native binary distribution (a package that ships its own exe):
///   `<dir>/node_modules/<pkg>/bin/<stem>.exe`. Returned as
///   `(<exe>, [])` — no node prefix.
/// - JS entrypoint (a JS-only package): `<dir>/node_modules/<pkg>/
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
            // Scoped: walk one more level (e.g. node_modules/@scope/agent-cli/bin/agent.js).
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

/// The host lookups shell detection and resolution make. [`ShellProbe::HOST`]
/// reads the real environment, PATH and disk; tests inject fakes.
pub(crate) struct ShellProbe<'a> {
    /// An environment variable's value.
    pub(crate) env: &'a dyn Fn(&str) -> Option<OsString>,
    /// A program name's path on PATH.
    pub(crate) which: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// Whether a file exists.
    pub(crate) exists: &'a dyn Fn(&Path) -> bool,
}

fn host_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

fn host_which(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

fn host_exists(path: &Path) -> bool {
    path.is_file()
}

impl ShellProbe<'static> {
    /// The real environment, PATH and disk.
    pub(crate) const HOST: Self = Self {
        env: &host_env,
        which: &host_which,
        exists: &host_exists,
    };
}

/// Which platform's shell list and lookups apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostOs {
    Windows,
    Unix,
}

impl HostOs {
    /// The platform this daemon runs on.
    pub(crate) const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };

    /// The named shells a user can choose here, as `(id, label)`, in the
    /// Settings order between Automatic and Custom.
    fn named_shells(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Windows => &[
                ("pwsh", "PowerShell 7"),
                ("powershell", "Windows PowerShell"),
                ("cmd", "Command Prompt"),
                ("git-bash", "Git Bash"),
                ("wsl", "WSL"),
                ("nu", "Nushell"),
            ],
            Self::Unix => &[
                ("bash", "bash"),
                ("zsh", "zsh"),
                ("fish", "fish"),
                ("nu", "Nushell"),
                ("sh", "sh"),
            ],
        }
    }
}

/// The choice id for Automatic.
const AUTO_SHELL_ID: &str = "auto";
/// The choice id for a custom command line.
const CUSTOM_SHELL_ID: &str = "custom";

/// Whether `id` is a choice this build offers on `os`: Automatic, Custom or
/// one of its named shells. Any other id reads as Automatic.
pub(crate) fn is_known_shell_id(os: HostOs, id: &str) -> bool {
    id == AUTO_SHELL_ID
        || id == CUSTOM_SHELL_ID
        || os.named_shells().iter().any(|(known, _)| *known == id)
}

/// Where the named shell `id` lives on this host, `None` when it is not
/// installed. Git Bash comes from the Git install (never `which bash`, which
/// can return WSL's launcher), WSL from `%SystemRoot%\System32\wsl.exe`, and
/// every other shell from PATH.
fn locate_shell(os: HostOs, id: &str, probe: &ShellProbe<'_>) -> Option<PathBuf> {
    match (os, id) {
        (HostOs::Windows, "git-bash") => locate_git_bash(probe),
        (HostOs::Windows, "wsl") => {
            let wsl = PathBuf::from((probe.env)("SystemRoot")?)
                .join("System32")
                .join("wsl.exe");
            (probe.exists)(&wsl).then_some(wsl)
        }
        (HostOs::Windows, name) => (probe.which)(&format!("{name}.exe")),
        (HostOs::Unix, name) => (probe.which)(name),
    }
}

/// Git Bash's `bash.exe`: beside the `git.exe` on PATH (`<root>\cmd\git.exe`
/// gives `<root>\bin\bash.exe`), else under `%ProgramFiles%\Git`, else under
/// `%LOCALAPPDATA%\Programs\Git`.
fn locate_git_bash(probe: &ShellProbe<'_>) -> Option<PathBuf> {
    let beside_git = (probe.which)("git.exe")
        .and_then(|git| Some(git.parent()?.parent()?.join("bin").join("bash.exe")));
    let program_files = (probe.env)("ProgramFiles")
        .map(|root| PathBuf::from(root).join("Git").join("bin").join("bash.exe"));
    let local = (probe.env)("LOCALAPPDATA").map(|root| {
        PathBuf::from(root)
            .join("Programs")
            .join("Git")
            .join("bin")
            .join("bash.exe")
    });
    [beside_git, program_files, local]
        .into_iter()
        .flatten()
        .find(|path| (probe.exists)(path))
}

/// The shell Automatic picks: the program to run, its label, and the path it
/// resolved to.
struct AutomaticPick {
    program: String,
    label: String,
    path: PathBuf,
}

/// Automatic's pick on `os`: Windows tries `pwsh.exe`, `powershell.exe`,
/// `cmd.exe` on PATH; Unix tries `$SHELL`, then `bash`, `zsh`, `sh` on PATH.
fn automatic_pick(os: HostOs, probe: &ShellProbe<'_>) -> Option<AutomaticPick> {
    let candidates: &[(&str, &str)] = match os {
        HostOs::Windows => &[
            ("pwsh.exe", "pwsh"),
            ("powershell.exe", "powershell"),
            ("cmd.exe", "cmd"),
        ],
        HostOs::Unix => {
            if let Some(p) = (probe.env)("SHELL").and_then(|value| value.into_string().ok()) {
                return Some(AutomaticPick {
                    label: shell_label_from_path(&p),
                    path: PathBuf::from(&p),
                    program: p,
                });
            }
            &[("bash", "bash"), ("zsh", "zsh"), ("sh", "sh")]
        }
    };
    candidates.iter().find_map(|(candidate, label)| {
        (probe.which)(candidate).map(|path| AutomaticPick {
            program: (*candidate).to_owned(),
            label: (*label).to_owned(),
            path,
        })
    })
}

/// The Automatic shell, with its integration.
fn automatic_shell(
    dirs: &Dirs,
    os: HostOs,
    probe: &ShellProbe<'_>,
) -> anyhow::Result<ShellProgram> {
    let Some(pick) = automatic_pick(os, probe) else {
        match os {
            HostOs::Windows => {
                anyhow::bail!("no shell on PATH (tried pwsh.exe, powershell.exe, cmd.exe)")
            }
            HostOs::Unix => anyhow::bail!("no shell on PATH (tried bash, zsh, sh)"),
        }
    };
    let (args, extra_env) = plain_shell_args(dirs, &pick.label)?;
    Ok(ShellProgram {
        program: pick.program,
        args,
        label: pick.label,
        extra_env,
    })
}

/// Whether an agent is typed into the shell at `path`.
fn hosts_agents(path: Option<&str>) -> bool {
    path.is_some_and(|path| ShellFamily::of(path).is_some())
}

/// Whether the saved custom shell command types an agent into a shell: it
/// splits into words, its program resolves, and the resolved program is a
/// shell family. This is the same split, resolution and family test a spawn of
/// the command makes, so Custom's flag agrees with what a spawn does.
fn custom_hosts_agents(os: HostOs, setting: Option<&DefaultShell>, probe: &ShellProbe<'_>) -> bool {
    let Some(command) = setting
        .filter(|setting| setting.id == CUSTOM_SHELL_ID)
        .and_then(|setting| setting.command.as_deref())
    else {
        return false;
    };
    let Some(words) = split_custom_command(os, command, probe).filter(|words| !words.is_empty())
    else {
        return false;
    };
    let Some(path) = resolve_custom_program(&words[0], probe) else {
        return false;
    };
    ShellFamily::of(&path.to_string_lossy()).is_some()
}

/// Every default-shell choice on this host, in Settings order: Automatic
/// (with the shell it resolves to now), the named shells with what was found,
/// and Custom, which hosts agents when `setting`'s saved custom command
/// resolves to a shell family. Blocking: it searches PATH and probes files.
pub(crate) fn detect_shells(
    setting: Option<&DefaultShell>,
    probe: &ShellProbe<'_>,
) -> Vec<ShellChoice> {
    detect_shells_on(HostOs::CURRENT, setting, probe)
}

fn detect_shells_on(
    os: HostOs,
    setting: Option<&DefaultShell>,
    probe: &ShellProbe<'_>,
) -> Vec<ShellChoice> {
    let lossy = |path: PathBuf| path.to_string_lossy().into_owned();
    let auto_path = automatic_pick(os, probe).map(|pick| lossy(pick.path));
    let mut choices = vec![ShellChoice {
        id: AUTO_SHELL_ID.to_owned(),
        label: "Automatic".to_owned(),
        available: true,
        hosts_agents: hosts_agents(auto_path.as_deref()),
        path: auto_path,
    }];
    for (id, label) in os.named_shells() {
        let path = locate_shell(os, id, probe).map(lossy);
        choices.push(ShellChoice {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
            available: path.is_some(),
            hosts_agents: hosts_agents(path.as_deref()),
            path,
        });
    }
    choices.push(ShellChoice {
        id: CUSTOM_SHELL_ID.to_owned(),
        label: "Custom".to_owned(),
        available: true,
        path: None,
        hosts_agents: custom_hosts_agents(os, setting, probe),
    });
    choices
}

/// Pick a shell executable for [`SessionMode::PlainShell`] spawns and for the
/// shell an interactive agent is typed into. `program` is what we pass to
/// `CommandBuilder` (resolved on PATH if it has no separator); `label` is the
/// short token shown in the UI; `args` and `extra_env` together install the
/// shell-integration init script for shells that support it (PowerShell /
/// bash / zsh) — emits OSC 7 for cwd and OSC 133 + OSC 633 marks the frontend
/// parses for the per-command chip UI. cmd.exe gets a minimal OSC 7 prompt
/// hook only (no command tracking).
///
/// Resolution order:
/// - `RUSTLING_TULIP_SHELL` env override (any platform); `setting` is not
///   consulted;
/// - the host's default shell, `setting`, when it is set and available (a
///   chosen shell not found, or a custom command that does not split or whose
///   program does not resolve, logs a warning and falls through); WSL starts
///   in `cwd`. When `types_resume`, a chosen shell with no [`ShellFamily`]
///   cannot run the resume line typed into it, so it falls through too;
/// - Automatic: Windows `pwsh.exe` → `powershell.exe` → `cmd.exe`; Unix
///   `$SHELL` → `bash` → `zsh` → `sh`.
pub(super) fn resolve_shell_program(
    dirs: &Dirs,
    setting: Option<&DefaultShell>,
    cwd: Option<&Path>,
    types_resume: bool,
) -> anyhow::Result<ShellProgram> {
    resolve_shell_with(
        dirs,
        setting,
        cwd,
        types_resume,
        HostOs::CURRENT,
        &ShellProbe::HOST,
    )
}

fn resolve_shell_with(
    dirs: &Dirs,
    setting: Option<&DefaultShell>,
    cwd: Option<&Path>,
    types_resume: bool,
    os: HostOs,
    probe: &ShellProbe<'_>,
) -> anyhow::Result<ShellProgram> {
    if let Some(p) = (probe.env)("RUSTLING_TULIP_SHELL").and_then(|value| value.into_string().ok())
    {
        let label = shell_label_from_path(&p);
        let (args, extra_env) = plain_shell_args(dirs, &label)?;
        return Ok(ShellProgram {
            program: p,
            args,
            label,
            extra_env,
        });
    }
    if let Some(setting) = setting
        && let Some(shell) = chosen_shell(dirs, setting, cwd, os, probe)?
    {
        if !types_resume || ShellFamily::of(&shell.program).is_some() {
            return Ok(shell);
        }
        tracing::info!(
            id = %setting.id,
            "shell: chosen shell cannot host the resume line; using automatic"
        );
    }
    automatic_shell(dirs, os, probe)
}

/// Split a custom shell command into its program and arguments by `os`'s
/// rules; `None` when it does not split (an unclosed quote). A command that,
/// trimmed and with one pair of surrounding `"` removed, names an existing
/// file is that program alone, so an unquoted path with spaces works. Windows
/// splits by the `CommandLineToArgvW` rules ([`split_windows_command_line`]),
/// so backslashes in paths are kept; Unix by POSIX shell words.
pub(crate) fn split_custom_command(
    os: HostOs,
    command: &str,
    probe: &ShellProbe<'_>,
) -> Option<Vec<String>> {
    let trimmed = command.trim();
    let whole = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    if !whole.is_empty() && (probe.exists)(Path::new(whole)) {
        return Some(vec![whole.to_owned()]);
    }
    match os {
        HostOs::Windows => split_windows_command_line(command),
        HostOs::Unix => shell_words::split(command).ok(),
    }
}

/// Split `line` by the Windows command-line rules: spaces and tabs separate
/// words and `"` groups them; a backslash is literal unless it comes before a
/// `"`, where 2n backslashes give n and toggle quoting and 2n+1 give n and a
/// literal `"`; `'` is an ordinary character. `None` when a `"` is left open.
fn split_windows_command_line(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut backslashes = 0usize;
    for c in line.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                in_word = true;
            }
            '"' => {
                word.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 == 1 {
                    word.push('"');
                } else {
                    quoted = !quoted;
                }
                backslashes = 0;
                in_word = true;
            }
            ' ' | '\t' if !quoted => {
                word.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                word.push(c);
                in_word = true;
            }
        }
    }
    if quoted {
        return None;
    }
    word.extend(std::iter::repeat_n('\\', backslashes));
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// Log that the chosen shell cannot run, so Automatic runs instead.
fn warn_shell_unavailable(id: &str, path: Option<&str>) {
    tracing::warn!(id = %id, path = ?path, "shell: chosen shell unavailable; using automatic");
}

/// The shell `setting` chooses; `None` for Automatic, an id this build does
/// not know, a named shell not found, and a custom command that does not
/// split into words. A custom command with arguments runs exactly as given;
/// without, it gets its program stem's integration.
fn chosen_shell(
    dirs: &Dirs,
    setting: &DefaultShell,
    cwd: Option<&Path>,
    os: HostOs,
    probe: &ShellProbe<'_>,
) -> anyhow::Result<Option<ShellProgram>> {
    let id = setting.id.as_str();
    if id == CUSTOM_SHELL_ID {
        let command = setting.command.as_deref().unwrap_or_default();
        let Some(mut words) =
            split_custom_command(os, command, probe).filter(|words| !words.is_empty())
        else {
            warn_shell_unavailable(id, setting.command.as_deref());
            return Ok(None);
        };
        let word = words.remove(0);
        let Some(path) = resolve_custom_program(&word, probe) else {
            warn_shell_unavailable(id, Some(&word));
            return Ok(None);
        };
        let program = path.to_string_lossy().into_owned();
        let label = shell_label_from_path(&program);
        let (args, extra_env) = if !words.is_empty() {
            (words, Vec::new())
        } else if runs_through_wsl(&program) {
            // WSL's bash or sh takes Linux paths; the integration scripts
            // are staged at Windows paths.
            (Vec::new(), Vec::new())
        } else {
            plain_shell_args(dirs, &label)?
        };
        return Ok(Some(ShellProgram {
            program,
            args,
            label,
            extra_env,
        }));
    }
    if id == AUTO_SHELL_ID || !is_known_shell_id(os, id) {
        return Ok(None);
    }
    let Some(path) = locate_shell(os, id, probe) else {
        warn_shell_unavailable(id, None);
        return Ok(None);
    };
    let program = path.to_string_lossy().into_owned();
    if id == "wsl" {
        let args = cwd.map_or_else(Vec::new, |cwd| {
            vec!["--cd".to_owned(), cwd.to_string_lossy().into_owned()]
        });
        return Ok(Some(ShellProgram {
            program,
            args,
            label: "wsl".to_owned(),
            extra_env: Vec::new(),
        }));
    }
    // Git Bash runs as bash, so it gets bash's integration.
    let label = if id == "git-bash" { "bash" } else { id }.to_owned();
    let (args, extra_env) = plain_shell_args(dirs, &label)?;
    Ok(Some(ShellProgram {
        program,
        args,
        label,
        extra_env,
    }))
}

/// Where a custom command's program `word` lives: a path with a separator
/// must exist; a bare name is looked up on PATH. `None` when it is empty or
/// not found.
fn resolve_custom_program(word: &str, probe: &ShellProbe<'_>) -> Option<PathBuf> {
    if word.is_empty() {
        return None;
    }
    if word.contains(['/', '\\']) {
        let path = PathBuf::from(word);
        return (probe.exists)(&path).then_some(path);
    }
    (probe.which)(word)
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
    Fish,
    Nushell,
}

impl ShellFamily {
    /// The family of the shell `program` runs, from its file stem: `pwsh` and
    /// `powershell` are PowerShell, `cmd` is cmd, `sh`, `bash`, `zsh`, `dash`
    /// and `ksh` are POSIX sh, `fish` is fish and `nu` is Nushell. `None` for
    /// any other shell, and for a POSIX, fish or Nushell shell that Windows
    /// runs through WSL (under `System32` or `WindowsApps`), which takes Linux
    /// paths.
    pub(super) fn of(program: &str) -> Option<Self> {
        match program_stem(program).as_str() {
            "pwsh" | "powershell" => Some(Self::PowerShell),
            "cmd" => Some(Self::Cmd),
            "sh" | "bash" | "zsh" | "dash" | "ksh" if !runs_through_wsl(program) => {
                Some(Self::Posix)
            }
            "fish" if !runs_through_wsl(program) => Some(Self::Fish),
            "nu" if !runs_through_wsl(program) => Some(Self::Nushell),
            _ => None,
        }
    }

    /// The bytes that clear the shell's input line: Esc for PowerShell and
    /// cmd, Ctrl+U for the line editors of a POSIX shell, fish and Nushell.
    fn clear_line(self) -> &'static [u8] {
        match self {
            Self::PowerShell | Self::Cmd => b"\x1b",
            Self::Posix | Self::Fish | Self::Nushell => b"\x15",
        }
    }
}

/// `program`'s file stem, split on both separators so a path named with
/// another platform's separators reads the same wherever the daemon runs.
fn program_leaf_stem(program: &str) -> Option<&str> {
    let leaf = program
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    Path::new(leaf).file_stem().and_then(|stem| stem.to_str())
}

/// `program`'s file stem, lower-cased; empty when it has none.
fn program_stem(program: &str) -> String {
    program_leaf_stem(program)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// Whether `program` is a Windows path into the system or app-alias folders,
/// where a POSIX, fish or Nushell shell is WSL's.
fn runs_through_wsl(program: &str) -> bool {
    let lower = program.to_ascii_lowercase();
    lower.contains(r"\system32\") || lower.contains(r"\windowsapps\")
}

/// Whether the agent's arguments pass through a parser that mangles some of
/// them: Windows PowerShell 5.1 hosting the agent, or cmd running it (the
/// resolved agent program is `cmd.exe` or a batch file). `agent` is `None`
/// for a program word typed verbatim, which is not a path to inspect.
fn legacy_argument_passing(shell_program: &str, agent: Option<&str>) -> bool {
    program_stem(shell_program) == "powershell" || agent.is_some_and(runs_through_cmd)
}

/// Whether Windows runs `program` through cmd: `cmd.exe` itself, or a batch
/// file.
fn runs_through_cmd(program: &str) -> bool {
    program_stem(program) == "cmd" || is_batch_file(program)
}

/// Whether `program` names a `.cmd` or `.bat` file.
fn is_batch_file(program: &str) -> bool {
    let ext = Path::new(program)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("cmd" | "bat"))
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
        ShellFamily::Nushell => Some(format!("^{quoted}")),
        ShellFamily::Cmd | ShellFamily::Posix | ShellFamily::Fish => Some(quoted),
    }
}

/// `args`, each quoted for `family`, joined by spaces. `None` when cmd cannot
/// carry one (see [`cmd_refuses`]).
fn quote_args(family: ShellFamily, args: &[String]) -> Option<String> {
    let quoted: Option<Vec<String>> = match family {
        ShellFamily::PowerShell => Some(args.iter().map(|arg| powershell_quote(arg)).collect()),
        ShellFamily::Cmd => args.iter().map(|arg| cmd_quote(arg)).collect(),
        ShellFamily::Posix => Some(args.iter().map(|arg| posix_quote(arg)).collect()),
        ShellFamily::Fish => Some(args.iter().map(|arg| fish_quote(arg)).collect()),
        ShellFamily::Nushell => Some(args.iter().map(|arg| nu_quote(arg)).collect()),
    };
    Some(quoted?.join(" "))
}

/// The agent's command as `family` types it: `argv` quoted (see
/// [`quote_command_line`]), or the user's program word bare followed by
/// `argv` quoted, then the user's extra arguments verbatim, then the spawn's
/// own extra arguments quoted. `None` when cmd cannot carry an argument, the
/// extras hold `%` or `!`, which cmd expands, or the spawn has arguments the
/// extras could swallow or redirect (see [`extras_carry_spawn_args`]).
pub(super) fn typed_command(
    family: ShellFamily,
    argv: &[String],
    typed: TypedCommand<'_>,
) -> Option<String> {
    let extra = typed.extra.trim();
    if matches!(family, ShellFamily::Cmd) && extra.contains(['%', '!']) {
        return None;
    }
    let suffix = spawn_args_suffix(family, extra, typed.spawn_args)?;
    let mut line = match typed.program_word {
        None => quote_command_line(family, argv)?,
        Some(word) if argv.is_empty() => typed_program_word(family, word),
        Some(word) => format!(
            "{} {}",
            typed_program_word(family, word),
            quote_args(family, argv)?
        ),
    };
    if !extra.is_empty() {
        line.push(' ');
        line.push_str(extra);
    }
    if !suffix.is_empty() {
        line.push(' ');
        line.push_str(&suffix);
    }
    Some(line)
}

/// The spawn's own `args` as `family` types them after the trimmed
/// launch-command `extra`, quoted and joined (empty without any); `None` when
/// cmd cannot carry one or `extra` could swallow or redirect them (see
/// [`extras_carry_spawn_args`]), so the spawn falls back to the direct
/// launch.
fn spawn_args_suffix(family: ShellFamily, extra: &str, args: &[String]) -> Option<String> {
    if args.is_empty() {
        return Some(String::new());
    }
    if !extra.is_empty() && !extras_carry_spawn_args(extra) {
        return None;
    }
    quote_args(family, args)
}

/// Whether arguments typed after the verbatim launch-command `extra` still
/// reach the agent: the extras end in no comment or line continuation (a word
/// typed after them splits as itself) and hold no command separator,
/// backtick, redirect or PowerShell block comment (`;`, `|`, `&`, `` ` ``,
/// `<`, `>`, which `<#` starts with).
fn extras_carry_spawn_args(extra: &str) -> bool {
    if extra.contains([';', '|', '&', '`', '<', '>']) {
        return false;
    }
    shell_words::split(&format!("{extra} x"))
        .is_ok_and(|words| words.last().is_some_and(|word| word == "x"))
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
        ShellFamily::Fish => Some(format!("cd {} && {command}", fish_quote(cwd))),
        ShellFamily::Nushell => Some(format!("cd {}; {command}", nu_quote(cwd))),
    }
}

/// The user's program word as `family` types it: bare, as the user typed it,
/// except in Nushell, where a bare word reads as a Nushell command, so the
/// external call needs `^`.
fn typed_program_word(family: ShellFamily, word: &str) -> String {
    match family {
        ShellFamily::Nushell => format!("^{word}"),
        _ => word.to_owned(),
    }
}

/// A fish argument: single-quoted, each `\` written `\\` and each `'` written
/// `\'`, the two escapes fish reads inside single quotes (POSIX sh's
/// close-escape-reopen trick is not one of them).
fn fish_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\\', r"\\").replace('\'', r"\'"))
}

/// A Nushell argument: single-quoted, a raw string with no escapes, unless it
/// holds a `'`, which a single-quoted string cannot carry; then a raw string
/// with the fewest `#` that no `'` followed by that many `#`s inside it closes
/// early.
fn nu_quote(arg: &str) -> String {
    if !arg.contains('\'') {
        return format!("'{arg}'");
    }
    let mut hashes = 1;
    while arg.contains(&format!("'{}", "#".repeat(hashes))) {
        hashes += 1;
    }
    let bar = "#".repeat(hashes);
    format!("r{bar}'{arg}'{bar}")
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

/// The characters cmd.exe acts on in an argument it runs a batch file with:
/// `%` and `!` expand, `"` ends the quoting, `^` escapes, `&`, `|`, `<` and
/// `>` run or redirect, and a line break ends the command. Parentheses pass,
/// so a folder under `Program Files (x86)` does.
const CMD_SHIM_REFUSED: [char; 10] = ['%', '!', '"', '^', '&', '|', '<', '>', '\r', '\n'];

/// The first character of `arg` cmd.exe would act on when it runs a batch
/// file with it (see [`CMD_SHIM_REFUSED`]); `None` for an argument it passes
/// as it is.
fn cmd_shim_refuses(arg: &str) -> Option<char> {
    arg.chars().find(|ch| CMD_SHIM_REFUSED.contains(ch))
}

/// A direct spawn refused because cmd.exe runs the agent and would act on a
/// character of one of its arguments, which a quoted argv cannot escape.
#[derive(Debug)]
pub(super) struct CmdShimRefusal {
    /// The file name of the batch file cmd.exe runs, else of the program.
    shim: String,
    /// The first refused character found.
    character: char,
    /// What the refused argument is, as the refusal names it.
    argument: &'static str,
    /// The plugin's program override variable, which the hint names.
    program_env_var: &'static str,
}

impl From<CmdShimRefusal> for SpawnFailure {
    fn from(refusal: CmdShimRefusal) -> Self {
        let shown = match refusal.character {
            '\r' => "CR".to_owned(),
            '\n' => "LF".to_owned(),
            ch => ch.to_string(),
        };
        Self {
            title: "Couldn't start session".to_owned(),
            detail: format!(
                "`{}` runs through cmd.exe, which would run or rewrite the `{shown}` in {}.",
                refusal.shim, refusal.argument
            ),
            hint: Some(format!(
                "Remove % ! \" ^ & | < >, or point the launch command or {} at the agent's .exe.",
                refusal.program_env_var
            )),
        }
    }
}

/// How a refusal names the program and the resolver's leading arguments,
/// which carry the batch file's path.
const AGENT_PATH: &str = "the agent's path";

/// The first argument of a direct spawn that cmd.exe would act on, as its
/// refused character and its name: `program`, `prepend` and `extra` by where
/// they come from, a plugin argument by the prompt it equals or the flag
/// before it.
fn refused_cmd_shim_arg(
    program: &str,
    prepend: &[String],
    plugin_args: &[String],
    extras: &[(&[String], &'static str)],
    initial_prompt: Option<&str>,
) -> Option<(char, &'static str)> {
    let leading = std::iter::once(program)
        .chain(prepend.iter().map(String::as_str))
        .map(|arg| (arg, AGENT_PATH));
    let own = plugin_args.iter().enumerate().map(|(at, arg)| {
        let previous = at
            .checked_sub(1)
            .and_then(|before| plugin_args.get(before))
            .map(String::as_str);
        (arg.as_str(), plugin_arg_name(arg, previous, initial_prompt))
    });
    let extras = extras
        .iter()
        .flat_map(|(args, name)| args.iter().map(|arg| (arg.as_str(), *name)));
    leading
        .chain(own)
        .chain(extras)
        .find_map(|(arg, name)| cmd_shim_refuses(arg).map(|ch| (ch, name)))
}

/// How a refusal names the plugin argument `arg`, which follows `previous`.
fn plugin_arg_name(
    arg: &str,
    previous: Option<&str>,
    initial_prompt: Option<&str>,
) -> &'static str {
    if initial_prompt == Some(arg) {
        return "the initial prompt";
    }
    match previous {
        Some("--model") => "the model",
        Some("--add-dir") => "the --add-dir path",
        Some("--settings") => "the --settings path",
        _ => "an argument",
    }
}

/// The batch file `program` runs with `prepend`: the first `.cmd` or `.bat`
/// among them, else `program`.
fn cmd_shim_path<'a>(program: &'a str, prepend: &'a [String]) -> &'a str {
    std::iter::once(program)
        .chain(prepend.iter().map(String::as_str))
        .find(|candidate| is_batch_file(candidate))
        .unwrap_or(program)
}

/// The file name of `path`. Either separator ends a folder, since the path is
/// a Windows one wherever the daemon reads it.
fn file_name_of(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
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
    /// The program the agent runs, resolved, whether or not the line types it
    /// (a program word typed verbatim resolves to it too).
    pub(super) program: &'a str,
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
    /// The spawn's own extra arguments, already split, each quoted for the
    /// shell and typed after [`Self::extra`].
    pub(super) spawn_args: &'a [String],
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
    /// The spawn's own extra arguments, passed after [`Self::extra_args`].
    pub(super) spawn_args: Vec<String>,
    /// The plugin's program override variable, which a refusal's hint names.
    pub(super) program_env_var: &'static str,
}

impl AgentCommandChoice {
    /// This choice with the spawn's own extra arguments `args`, which follow
    /// the launch command's.
    #[must_use]
    pub(super) fn with_spawn_args(mut self, args: &[String]) -> Self {
        self.spawn_args = args.to_vec();
        self
    }

    /// The direct spawn's arguments for the resolved `program`: the
    /// resolver's leading `prepend`, the plugin's own options, the launch
    /// command's extra arguments, the spawn's own, then the plugin's `--`
    /// separator and what follows it (`initial_prompt`). User arguments stay
    /// ahead of the separator so the program never reads one as part of the
    /// prompt.
    ///
    /// # Errors
    ///
    /// `program` runs through cmd.exe and an argument holds a character cmd
    /// would act on (see [`cmd_shim_refuses`]).
    pub(super) fn direct_args(
        &self,
        program: &str,
        prepend: Vec<String>,
        plugin_args: Vec<String>,
        initial_prompt: Option<&str>,
    ) -> Result<Vec<String>, CmdShimRefusal> {
        if runs_through_cmd(program)
            && let Some((character, argument)) = refused_cmd_shim_arg(
                program,
                &prepend,
                &plugin_args,
                &[
                    (&self.extra_args, "a launch-command argument"),
                    (&self.spawn_args, "an extra argument of this spawn"),
                ],
                initial_prompt,
            )
        {
            // A refused character in the path itself shows the whole path, so
            // the user sees the folder that holds it.
            let shim = cmd_shim_path(program, &prepend);
            let shown = if argument == AGENT_PATH {
                shim
            } else {
                file_name_of(shim)
            };
            return Err(CmdShimRefusal {
                shim: shown.to_owned(),
                character,
                argument,
                program_env_var: self.program_env_var,
            });
        }
        let (options, after_separator) = match plugin_args.iter().position(|arg| arg == "--") {
            Some(at) => (plugin_args[..at].to_vec(), plugin_args[at..].to_vec()),
            None => (plugin_args, Vec::new()),
        };
        let mut args = prepend;
        args.extend(options);
        args.extend(self.extra_args.iter().cloned());
        args.extend(self.spawn_args.iter().cloned());
        args.extend(after_separator);
        Ok(args)
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
        spawn_args: Vec::new(),
        program_env_var: plugin.program_env_var(),
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
    // The spawn's own arguments are quoted like the plugin's, so the same
    // checks read them.
    let quoted_args = || agent.argv.iter().chain(agent.typed.spawn_args);
    let legacy_refused = legacy_argument_passing(&shell.program, resolved_program)
        && quoted_args().any(|arg| mangled_by_legacy_parsing(arg));
    // A shell other than cmd hands the arguments to cmd.exe as they are, and
    // cmd would act on its metacharacters; the direct spawn refuses them.
    // `agent.program` is checked, not argv's first word, so a program word
    // typed verbatim that resolves to a batch file counts too.
    let cmd_refused = family != ShellFamily::Cmd
        && runs_through_cmd(agent.program)
        && quoted_args().any(|arg| cmd_shim_refuses(arg).is_some());
    let line = typed_command(family, agent.argv, agent.typed)
        .and_then(|command| launch_line(family, agent.cwd, &command))
        .filter(|_| !legacy_refused && !cmd_refused);
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
    program_leaf_stem(path).map_or_else(|| "shell".to_string(), str::to_lowercase)
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
        AgentLaunch, Launch, ShellFamily, ShellProgram, TypedCommand, cmd_shim_refuses, fish_quote,
        launch_line, nu_quote, plan_launch, quote_args, quote_command_line, typed_command,
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
            program: argv.first().map_or("", String::as_str),
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
            spawn_args: &[],
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
                spawn_args: &[],
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
            spawn_args: &[],
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
    fn a_typed_line_quotes_the_spawns_extra_args_after_the_launch_command_extras() {
        let plugin_args = strings(&["--model", "opus"]);
        let spawn_args = strings(&["--name", "a b"]);
        let typed = TypedCommand {
            program_word: Some("cl"),
            extra: " --debug ",
            spawn_args: &spawn_args,
        };
        assert_eq!(
            typed_line_with(shell("pwsh.exe", "pwsh"), &plugin_args, typed).as_deref(),
            Some(
                r"Set-Location -LiteralPath 'C:\work\repo'; cl '--model' 'opus' --debug '--name' 'a b'"
            ),
            "PowerShell quotes each of the spawn's arguments after the verbatim extras"
        );
        assert_eq!(
            typed_line_with(shell("cmd.exe", "cmd"), &plugin_args, typed).as_deref(),
            Some(r#"cd /d "C:\work\repo" && cl --model opus --debug --name "a b""#)
        );
        assert_eq!(
            typed_line_with(shell("/bin/bash", "bash"), &plugin_args, typed).as_deref(),
            Some(r"cd -- 'C:\work\repo' && cl '--model' 'opus' --debug '--name' 'a b'")
        );
        let blank = TypedCommand {
            program_word: None,
            extra: "",
            spawn_args: &spawn_args,
        };
        assert_eq!(
            typed_line_with(
                shell("pwsh.exe", "pwsh"),
                &strings(&[r"C:\bin\claude.exe", "--model", "opus"]),
                blank
            )
            .as_deref(),
            Some(
                r"Set-Location -LiteralPath 'C:\work\repo'; & 'C:\bin\claude.exe' '--model' 'opus' '--name' 'a b'"
            ),
            "without launch-command extras they follow the agent's own arguments"
        );
    }

    #[test]
    fn a_spawns_extra_args_the_shell_cannot_carry_fall_back_to_the_direct_launch() {
        let plugin_args = strings(&["--model", "opus"]);
        let percent = strings(&["--name", "%USERNAME%"]);
        let in_cmd = TypedCommand {
            program_word: Some("cl"),
            extra: "",
            spawn_args: &percent,
        };
        assert!(
            typed_line_with(shell("cmd.exe", "cmd"), &plugin_args, in_cmd).is_none(),
            "cmd would expand the spawn's argument"
        );
        let quote = strings(&[r#"say "hi""#]);
        let legacy = TypedCommand {
            program_word: Some("cl"),
            extra: "",
            spawn_args: &quote,
        };
        assert!(
            typed_line_with(shell("powershell.exe", "powershell"), &plugin_args, legacy).is_none(),
            "Windows PowerShell's legacy argument passing would mangle the spawn's argument"
        );
        let amp = strings(&[r"C:\dev\R&D"]);
        let through_cmd = AgentLaunch {
            program: r"C:\tools\mytool.cmd",
            typed: TypedCommand {
                program_word: Some("mytool"),
                extra: "",
                spawn_args: &amp,
            },
            ..agent(&plugin_args, None)
        };
        assert!(
            matches!(
                plan_launch(true, Some(shell("pwsh.exe", "pwsh")), through_cmd),
                Launch::Direct
            ),
            "pwsh hands the spawn's & to the cmd.exe that runs mytool.cmd"
        );
        let spawn_args = strings(&["--name", "a"]);
        let families = [
            (ShellFamily::PowerShell, "--debug `"),
            (ShellFamily::Posix, r"--debug \"),
            (ShellFamily::Fish, r"--debug \"),
            (ShellFamily::Nushell, r"--debug \"),
        ];
        for (family, continuation) in families {
            for extra in [
                "--debug #note",
                "--debug ;",
                "--debug |",
                "--debug &",
                "--debug <# note",
                "--debug >",
                "--debug 2>",
                continuation,
            ] {
                let typed = TypedCommand {
                    program_word: Some("cl"),
                    extra,
                    spawn_args: &spawn_args,
                };
                assert_eq!(
                    typed_command(family, &plugin_args, typed),
                    None,
                    "{family:?} extras {extra:?} would swallow or redirect the spawn's arguments"
                );
            }
            let plain = TypedCommand {
                program_word: Some("cl"),
                extra: "--debug",
                spawn_args: &spawn_args,
            };
            let line = typed_command(family, &plugin_args, plain)
                .unwrap_or_else(|| panic!("{family:?} types plain extras"));
            assert!(
                line.ends_with(&format!(
                    "--debug {}",
                    quote_args(family, &spawn_args).unwrap_or_default()
                )),
                "{family:?}: the spawn's arguments follow plain extras: {line}"
            );
        }
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
                    spawn_args: &[],
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
    fn cmd_shim_refuses_its_metacharacters_but_not_parentheses() {
        for ch in ['%', '!', '"', '^', '&', '|', '<', '>', '\r', '\n'] {
            let arg = format!("fix a {ch} b");
            assert_eq!(cmd_shim_refuses(&arg), Some(ch), "cmd.exe acts on {ch:?}");
        }
        for arg in ["(", ")", "fix the bug", r"C:\Program Files (x86)\x", ""] {
            assert_eq!(cmd_shim_refuses(arg), None, "cmd.exe passes {arg:?}");
        }
        assert_eq!(
            cmd_shim_refuses("a|b&c"),
            Some('|'),
            "the first refused character is the one named"
        );
    }

    #[test]
    fn a_cmd_run_agent_typed_into_powershell_falls_back_on_a_cmd_metacharacter() {
        for program in [
            &["cmd.exe", "/d", "/c", r"C:\bin\claude.cmd"][..],
            &[r"C:\bin\claude.cmd"],
        ] {
            let mut argv = strings(program);
            argv.extend(strings(&["--model", "a&b"]));
            assert!(
                typed_line(shell("pwsh.exe", "pwsh"), &argv).is_none(),
                "cmd.exe running {program:?} would run what follows the &"
            );
            assert!(
                typed_line(shell("/bin/bash", "bash"), &argv).is_none(),
                "bash hands {program:?} the & for cmd.exe to run"
            );
        }
        let exe = strings(&[r"C:\bin\claude.exe", "--model", "a&b"]);
        assert!(
            typed_line(shell("pwsh.exe", "pwsh"), &exe).is_some(),
            "an .exe agent takes the & as it is"
        );
    }

    #[test]
    fn a_verbatim_word_resolving_to_a_cmd_shim_falls_back_on_a_cmd_metacharacter() {
        let plugin_args = strings(&["--add-dir", r"C:\dev\R&D"]);
        let typed = TypedCommand {
            program_word: Some("mytool"),
            extra: "",
            spawn_args: &[],
        };
        let through_cmd = AgentLaunch {
            program: r"C:\tools\mytool.cmd",
            typed,
            ..agent(&plugin_args, None)
        };
        assert!(
            matches!(
                plan_launch(true, Some(shell("pwsh.exe", "pwsh")), through_cmd),
                Launch::Direct
            ),
            "pwsh hands the & to the cmd.exe that runs mytool.cmd"
        );
        let exe = AgentLaunch {
            program: r"C:\tools\mytool.exe",
            typed,
            ..agent(&plugin_args, None)
        };
        assert!(
            matches!(
                plan_launch(true, Some(shell("pwsh.exe", "pwsh")), exe),
                Launch::InShell(_)
            ),
            "a word resolving to an .exe takes the & as it is"
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
    fn fish_quotes_backslashes_and_single_quotes() {
        assert_eq!(fish_quote(r"a\b'c"), r"'a\\b\'c'");
        assert_eq!(fish_quote(r"C:\x y"), r"'C:\\x y'");
        assert_eq!(
            quote_command_line(ShellFamily::Fish, &strings(&[r"a\b'c", "plain"])).as_deref(),
            Some(r"'a\\b\'c' 'plain'"),
            "fish joins the quoted arguments with no prefix"
        );
    }

    #[test]
    fn nu_quotes_raw_when_a_single_quote_is_inside() {
        assert_eq!(nu_quote("it's"), r"r#'it's'#");
        assert_eq!(nu_quote("a'#b"), r"r##'a'#b'##");
        assert_eq!(nu_quote("plain"), "'plain'");
        assert_eq!(
            nu_quote(r"C:\x y"),
            r"'C:\x y'",
            "a raw string keeps backslashes"
        );
        assert_eq!(
            quote_command_line(ShellFamily::Nushell, &strings(&["plain", "--resume", "x"]))
                .as_deref(),
            Some(r"^'plain' '--resume' 'x'"),
            "an external call carries the ^ prefix"
        );
    }

    #[test]
    fn fish_launch_line_cds_then_runs() {
        let argv = strings(&[r"C:\bin\claude.exe", "--resume", "x"]);
        let command = quote_command_line(ShellFamily::Fish, &argv).expect("fish quotes it");
        assert_eq!(
            launch_line(ShellFamily::Fish, r"C:\work\my repo", &command).as_deref(),
            Some(r"cd 'C:\\work\\my repo' && 'C:\\bin\\claude.exe' '--resume' 'x'")
        );
    }

    #[test]
    fn nushell_launch_line_cds_then_calls_external() {
        let argv = strings(&[r"C:\bin\claude.exe", "--resume", "x"]);
        let command = quote_command_line(ShellFamily::Nushell, &argv).expect("Nushell quotes it");
        assert_eq!(
            launch_line(ShellFamily::Nushell, r"C:\work\my repo", &command).as_deref(),
            Some(r"cd 'C:\work\my repo'; ^'C:\bin\claude.exe' '--resume' 'x'")
        );
    }

    #[test]
    fn fish_and_nu_are_recognised_and_wsl_ones_are_not() {
        for fish in ["fish", "/usr/bin/fish", r"C:\Program Files\fish\fish.exe"] {
            assert_eq!(ShellFamily::of(fish), Some(ShellFamily::Fish), "{fish}");
        }
        for nu in ["nu", "/usr/bin/nu", r"C:\Tools\nu.exe"] {
            assert_eq!(ShellFamily::of(nu), Some(ShellFamily::Nushell), "{nu}");
        }
        for through_wsl in [
            r"C:\Windows\System32\fish.exe",
            r"C:\Users\a\AppData\Local\Microsoft\WindowsApps\nu.exe",
        ] {
            assert_eq!(
                ShellFamily::of(through_wsl),
                None,
                "{through_wsl} runs through WSL"
            );
        }
    }

    #[test]
    fn fish_and_nushell_clear_with_ctrl_u() {
        for family in [ShellFamily::Fish, ShellFamily::Nushell] {
            assert_eq!(
                family.clear_line(),
                [0x15_u8].as_slice(),
                "{family:?} clears to the start of its line"
            );
        }
    }

    #[test]
    fn an_unknown_shell_keeps_the_direct_spawn() {
        for unknown in [
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
            program: r"C:\bin\claude.exe",
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

    /// Git Bash classifies as POSIX by its stem, so an agent is typed into it
    /// with a POSIX line, Windows paths kept verbatim inside the quotes.
    #[cfg(windows)]
    #[test]
    fn git_bash_launch_changes_to_the_start_folder_first() {
        let argv = strings(&[r"C:\bin\claude.exe", "--model", "opus"]);
        assert_eq!(
            typed_line(shell(r"C:\Program Files\Git\bin\bash.exe", "bash"), &argv).as_deref(),
            Some(r"cd -- 'C:\work\repo' && 'C:\bin\claude.exe' '--model' 'opus'")
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
                program: r"C:\bin\claude.exe",
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
    use crate::spawn_plan::SpawnFailure;
    use crate::state::AgentCommand;

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
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
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
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
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

        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
        let chosen =
            choose_agent_command(plugin, &command(&path, ""), &no_env).expect("the command splits");
        assert_eq!(chosen.program, path);
        assert!(!chosen.typed_verbatim, "a path is not an alias");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_command_extras_follow_the_plugin_argv() {
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
        let chosen = choose_agent_command(plugin, &command("", r#"--debug "a b" 'c d'"#), &no_env)
            .expect("the command splits");
        let argv = chosen
            .direct_args(
                "cmd.exe",
                vec!["/d".to_owned(), "/c".to_owned()],
                vec!["--model".to_owned(), "opus".to_owned()],
                None,
            )
            .expect("no argument holds a cmd metacharacter");
        assert_eq!(
            argv,
            ["/d", "/c", "--model", "opus", "--debug", "a b", "c d"]
        );
    }

    #[test]
    fn a_spawns_extra_args_follow_the_launch_command_extras() {
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
        let chosen = choose_agent_command(plugin, &command("", "--debug"), &no_env)
            .expect("the command splits")
            .with_spawn_args(&["--name".to_owned(), "a b".to_owned()]);
        let argv = chosen
            .direct_args(
                r"C:\bin\claude.exe",
                Vec::new(),
                vec!["--model".to_owned(), "opus".to_owned()],
                None,
            )
            .expect("an .exe takes every argument");
        assert_eq!(
            argv,
            ["--model", "opus", "--debug", "--name", "a b"],
            "the plugin's argv, then the host's extras, then the spawn's own"
        );

        let refused = choose_agent_command(plugin, &command("", ""), &no_env)
            .expect("a blank command splits")
            .with_spawn_args(&[r"C:\dev\R&D".to_owned()])
            .direct_args(r"C:\npm\claude.cmd", Vec::new(), Vec::new(), None)
            .expect_err("cmd.exe would act on the spawn's &");
        assert_eq!(refused.argument, "an extra argument of this spawn");
    }

    #[test]
    fn launch_arguments_go_before_the_prompt_separator() {
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
        let chosen = choose_agent_command(plugin, &command("", "--debug"), &no_env)
            .expect("the command splits")
            .with_spawn_args(&["--name".to_owned(), "a b".to_owned()]);
        let argv = chosen
            .direct_args(
                r"C:\bin\claude.exe",
                Vec::new(),
                vec![
                    "--model".to_owned(),
                    "opus".to_owned(),
                    "--".to_owned(),
                    "hello".to_owned(),
                ],
                Some("hello"),
            )
            .expect("an .exe takes every argument");
        assert_eq!(
            argv,
            ["--model", "opus", "--debug", "--name", "a b", "--", "hello"],
            "the host's extras and the spawn's own go before the separator"
        );
    }

    #[test]
    fn a_cmd_shim_direct_spawn_refuses_an_ampersand_prompt() {
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
        let chosen =
            choose_agent_command(plugin, &command("", ""), &no_env).expect("the command splits");
        let prompt = "fix a & b";
        let shim = vec![
            "/d".to_owned(),
            "/c".to_owned(),
            r"C:\npm\claude.cmd".to_owned(),
        ];
        let plugin_args = vec!["--model".to_owned(), "opus".to_owned(), prompt.to_owned()];
        let refusal = chosen
            .direct_args("cmd.exe", shim, plugin_args.clone(), Some(prompt))
            .expect_err("cmd.exe would run what follows the &");
        let failure = SpawnFailure::from(refusal);
        assert_eq!(failure.title, "Couldn't start session");
        assert_eq!(
            failure.detail,
            "`claude.cmd` runs through cmd.exe, which would run or rewrite the `&` in the initial prompt."
        );
        assert_eq!(
            failure.hint.as_deref(),
            Some(
                "Remove % ! \" ^ & | < >, or point the launch command or RUSTLING_TULIP_CLAUDE at the agent's .exe."
            )
        );
        assert!(
            chosen
                .direct_args(r"C:\bin\claude.exe", Vec::new(), plugin_args, Some(prompt))
                .is_ok(),
            "an .exe agent takes the & as it is"
        );

        let extras = choose_agent_command(plugin, &command("", "--name a|b"), &no_env)
            .expect("the command splits");
        let refusal = extras
            .direct_args(r"C:\npm\claude.cmd", Vec::new(), Vec::new(), None)
            .expect_err("cmd.exe would pipe the extra argument");
        assert_eq!(
            SpawnFailure::from(refusal).detail,
            "`claude.cmd` runs through cmd.exe, which would run or rewrite the `|` in a launch-command argument."
        );
    }

    #[test]
    fn a_cmd_shim_path_holding_an_ampersand_is_named_in_full() {
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
        let chosen =
            choose_agent_command(plugin, &command("", ""), &no_env).expect("the command splits");
        let shim = r"C:\Users\R&D\AppData\Roaming\custom\claude.cmd";
        let prepend = vec!["/d".to_owned(), "/c".to_owned(), shim.to_owned()];
        let refusal = chosen
            .direct_args(
                "cmd.exe",
                prepend,
                vec!["--model".to_owned(), "opus".to_owned()],
                None,
            )
            .expect_err("cmd.exe would run what follows the & in the shim's folder");
        assert_eq!(
            SpawnFailure::from(refusal).detail,
            format!(
                "`{shim}` runs through cmd.exe, which would run or rewrite the `&` in the agent's path."
            )
        );
        let refusal = chosen
            .direct_args(shim, Vec::new(), Vec::new(), None)
            .expect_err("a batch file the spawn runs itself is checked too");
        assert_eq!(
            SpawnFailure::from(refusal).detail,
            format!(
                "`{shim}` runs through cmd.exe, which would run or rewrite the `&` in the agent's path."
            )
        );
    }

    #[test]
    fn launch_command_split_failure_refuses_the_spawn() {
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod default_shell_tests {
    use super::{
        HostOs, ShellFamily, ShellProbe, ShellProgram, detect_shells_on, resolve_shell_with,
        split_custom_command,
    };
    use crate::paths::Dirs;
    use protocol::{DefaultShell, ShellChoice};
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// A host with a fake environment, PATH and disk.
    #[derive(Default)]
    struct FakeHost {
        env: HashMap<&'static str, &'static str>,
        path: HashMap<&'static str, &'static str>,
        files: HashSet<PathBuf>,
    }

    impl FakeHost {
        fn env(mut self, name: &'static str, value: &'static str) -> Self {
            self.env.insert(name, value);
            self
        }

        fn on_path(mut self, name: &'static str, path: &'static str) -> Self {
            self.path.insert(name, path);
            self
        }

        fn file(mut self, path: PathBuf) -> Self {
            self.files.insert(path);
            self
        }

        fn probe<R>(&self, f: impl FnOnce(&ShellProbe<'_>) -> R) -> R {
            let env = |name: &str| self.env.get(name).map(|value| OsString::from(*value));
            let which = |name: &str| self.path.get(name).map(PathBuf::from);
            let exists = |path: &Path| self.files.contains(path);
            f(&ShellProbe {
                env: &env,
                which: &which,
                exists: &exists,
            })
        }

        fn choices(&self, os: HostOs) -> Vec<ShellChoice> {
            self.choices_with(os, None)
        }

        fn choices_with(&self, os: HostOs, setting: Option<&DefaultShell>) -> Vec<ShellChoice> {
            self.probe(|probe| detect_shells_on(os, setting, probe))
        }

        fn resolve(&self, setting: Option<&DefaultShell>, cwd: Option<&Path>) -> ShellProgram {
            self.resolve_typing(setting, cwd, false)
        }

        /// Resolve for a spawn that types a resume line when `resume`.
        fn resolve_typing(
            &self,
            setting: Option<&DefaultShell>,
            cwd: Option<&Path>,
            resume: bool,
        ) -> ShellProgram {
            let dirs = unused_dirs();
            self.probe(|probe| {
                resolve_shell_with(&dirs, setting, cwd, resume, HostOs::Windows, probe)
            })
            .expect("a shell resolves")
        }
    }

    /// Dirs no test here writes to: the shells they resolve stage no files.
    fn unused_dirs() -> Dirs {
        let root = std::env::temp_dir().join("rt-default-shell-unused");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        }
    }

    fn choice<'a>(choices: &'a [ShellChoice], id: &str) -> &'a ShellChoice {
        choices
            .iter()
            .find(|choice| choice.id == id)
            .expect("the choice is listed")
    }

    fn setting(id: &str, command: Option<&str>) -> DefaultShell {
        DefaultShell {
            id: id.to_owned(),
            command: command.map(str::to_owned),
        }
    }

    fn wsl_path() -> PathBuf {
        PathBuf::from(r"C:\Windows")
            .join("System32")
            .join("wsl.exe")
    }

    /// Windows-only: the `hosts_agents` asserts parse Windows paths.
    #[cfg(windows)]
    #[test]
    fn windows_choices_come_in_order_with_what_path_has() {
        let host = FakeHost::default()
            .on_path("pwsh.exe", r"C:\Program Files\PowerShell\7\pwsh.exe")
            .on_path("cmd.exe", r"C:\Windows\System32\cmd.exe");
        let choices = host.choices(HostOs::Windows);
        let listed: Vec<(&str, &str, bool)> = choices
            .iter()
            .map(|choice| (choice.id.as_str(), choice.label.as_str(), choice.available))
            .collect();
        assert_eq!(
            listed,
            [
                ("auto", "Automatic", true),
                ("pwsh", "PowerShell 7", true),
                ("powershell", "Windows PowerShell", false),
                ("cmd", "Command Prompt", true),
                ("git-bash", "Git Bash", false),
                ("wsl", "WSL", false),
                ("nu", "Nushell", false),
                ("custom", "Custom", true),
            ]
        );
        let pwsh = choice(&choices, "pwsh");
        assert_eq!(
            pwsh.path.as_deref(),
            Some(r"C:\Program Files\PowerShell\7\pwsh.exe")
        );
        assert!(pwsh.hosts_agents);
        assert_eq!(choice(&choices, "powershell").path, None);
        let custom = choice(&choices, "custom");
        assert_eq!(custom.path, None, "Custom has no path");
        assert!(!custom.hosts_agents, "nothing saved as Custom");
    }

    /// A saved Custom command that resolves to a shell family hosts agents,
    /// exactly as a spawn of it does.
    #[test]
    fn a_resolving_custom_command_reports_that_it_hosts_agents() {
        let host = FakeHost::default().on_path("pwsh", "/usr/bin/pwsh");
        let saved = setting("custom", Some("pwsh -NoLogo"));
        let choices = host.choices_with(HostOs::Unix, Some(&saved));
        let custom = choice(&choices, "custom");
        assert_eq!(custom.path, None, "Custom has no path");
        assert!(custom.available, "Custom is always available");
        assert!(custom.hosts_agents, "pwsh hosts agents");
    }

    #[test]
    fn no_saved_custom_command_reports_no_agents() {
        let host = FakeHost::default();
        assert!(!choice(&host.choices_with(HostOs::Unix, None), "custom").hosts_agents);
        let other = setting("pwsh", Some("pwsh -NoLogo"));
        assert!(
            !choice(&host.choices_with(HostOs::Unix, Some(&other)), "custom").hosts_agents,
            "only a saved Custom command counts"
        );
    }

    /// Windows-only: `ShellFamily::of` parses the resolved path as a Windows
    /// path to see that `System32\bash.exe` is WSL's.
    #[cfg(windows)]
    #[test]
    fn a_custom_bare_bash_under_system32_reports_no_agents() {
        let host = FakeHost::default().on_path("bash", r"C:\Windows\System32\bash.exe");
        let saved = setting("custom", Some("bash"));
        let choices = host.choices_with(HostOs::Windows, Some(&saved));
        let custom = choice(&choices, "custom");
        assert!(!custom.hosts_agents, "WSL's bash takes Linux paths");
        assert!(custom.available, "Custom is always available");
    }

    #[test]
    fn an_unresolvable_custom_command_reports_no_agents() {
        let host = FakeHost::default();
        let saved = setting("custom", Some("pwsh -NoLogo"));
        let choices = host.choices_with(HostOs::Windows, Some(&saved));
        assert!(!choice(&choices, "custom").hosts_agents);
    }

    #[cfg(windows)]
    #[test]
    fn git_bash_comes_from_the_git_install_never_from_a_bash_on_path() {
        let beside_git = PathBuf::from(r"D:\Git\bin\bash.exe");
        let program_files = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let local = PathBuf::from(r"C:\Users\u\AppData\Local\Programs\Git\bin\bash.exe");
        let git_bash = |host: &FakeHost| choice(&host.choices(HostOs::Windows), "git-bash").clone();

        let host = FakeHost::default()
            .on_path("git.exe", r"D:\Git\cmd\git.exe")
            .env("ProgramFiles", r"C:\Program Files")
            .file(beside_git.clone())
            .file(program_files.clone());
        let found = git_bash(&host);
        assert!(found.available);
        assert_eq!(
            found.path.as_deref(),
            Some(r"D:\Git\bin\bash.exe"),
            "beside git.exe first"
        );
        assert!(found.hosts_agents, "Git Bash hosts agents as a POSIX shell");

        let host = FakeHost::default()
            .env("ProgramFiles", r"C:\Program Files")
            .env("LOCALAPPDATA", r"C:\Users\u\AppData\Local")
            .file(program_files)
            .file(local.clone());
        assert_eq!(
            git_bash(&host).path.as_deref(),
            Some(r"C:\Program Files\Git\bin\bash.exe"),
            "then under Program Files"
        );

        let host = FakeHost::default()
            .env("ProgramFiles", r"C:\Program Files")
            .env("LOCALAPPDATA", r"C:\Users\u\AppData\Local")
            .file(local);
        assert_eq!(
            git_bash(&host).path.as_deref(),
            Some(r"C:\Users\u\AppData\Local\Programs\Git\bin\bash.exe"),
            "then under the user's Programs"
        );

        let host = FakeHost::default()
            .on_path("bash.exe", r"C:\Windows\System32\bash.exe")
            .on_path("bash", r"C:\Windows\System32\bash.exe");
        let missing = git_bash(&host);
        assert!(!missing.available, "a bash on PATH is not Git Bash");
        assert_eq!(missing.path, None);
    }

    #[test]
    fn wsl_is_available_when_its_launcher_exists() {
        let host = FakeHost::default().env("SystemRoot", r"C:\Windows");
        assert!(!choice(&host.choices(HostOs::Windows), "wsl").available);

        let host = host.file(wsl_path());
        let choices = host.choices(HostOs::Windows);
        let wsl = choice(&choices, "wsl");
        assert!(wsl.available);
        assert_eq!(wsl.path, Some(wsl_path().to_string_lossy().into_owned()));
    }

    #[test]
    fn unix_choices_come_in_order_with_what_path_has() {
        let host = FakeHost::default()
            .on_path("bash", "/usr/bin/bash")
            .on_path("fish", "/usr/bin/fish")
            .on_path("nu", "/usr/local/bin/nu")
            .on_path("sh", "/bin/sh");
        let choices = host.choices(HostOs::Unix);
        let listed: Vec<(&str, &str, bool, Option<&str>)> = choices
            .iter()
            .map(|choice| {
                (
                    choice.id.as_str(),
                    choice.label.as_str(),
                    choice.available,
                    choice.path.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("auto", "Automatic", true, Some("/usr/bin/bash")),
                ("bash", "bash", true, Some("/usr/bin/bash")),
                ("zsh", "zsh", false, None),
                ("fish", "fish", true, Some("/usr/bin/fish")),
                ("nu", "Nushell", true, Some("/usr/local/bin/nu")),
                ("sh", "sh", true, Some("/bin/sh")),
                ("custom", "Custom", true, None),
            ]
        );
        assert!(choice(&choices, "bash").hosts_agents);
        assert!(choice(&choices, "sh").hosts_agents);
    }

    /// Windows-only: the `hosts_agents` asserts parse Windows paths.
    #[cfg(windows)]
    #[test]
    fn automatic_names_the_shell_it_resolves_to() {
        let host = FakeHost::default().on_path(
            "powershell.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        );
        let choices = host.choices(HostOs::Windows);
        let auto = choice(&choices, "auto");
        assert_eq!(
            auto.path.as_deref(),
            Some(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        );
        assert!(auto.hosts_agents);

        let host = FakeHost::default()
            .env("SHELL", "/bin/zsh")
            .on_path("bash", "/usr/bin/bash");
        let choices = host.choices(HostOs::Unix);
        assert_eq!(
            choice(&choices, "auto").path.as_deref(),
            Some("/bin/zsh"),
            "$SHELL comes first"
        );

        let choices = FakeHost::default().choices(HostOs::Windows);
        let auto = choice(&choices, "auto");
        assert!(auto.available, "Automatic is always offered");
        assert_eq!(auto.path, None, "nothing to resolve to");
        assert!(!auto.hosts_agents);
    }

    #[test]
    fn wsl_does_not_host_agents_yet() {
        let host = FakeHost::default()
            .env("SystemRoot", r"C:\Windows")
            .file(wsl_path())
            .on_path("nu.exe", r"C:\Tools\nu.exe");
        let choices = host.choices(HostOs::Windows);
        let wsl = choice(&choices, "wsl");
        assert!(wsl.available, "wsl is installed");
        assert!(!wsl.hosts_agents, "wsl spawns agents on their own");
        let nu = choice(&choices, "nu");
        assert!(nu.available, "nu is installed");
        assert!(nu.hosts_agents, "Nushell hosts agents");

        let host = FakeHost::default()
            .on_path("fish", "/usr/bin/fish")
            .on_path("nu", "/usr/bin/nu");
        let choices = host.choices(HostOs::Unix);
        for id in ["fish", "nu"] {
            let found = choice(&choices, id);
            assert!(found.available, "{id} is installed");
            assert!(found.hosts_agents, "{id} hosts agents");
        }
    }

    /// A Windows host with PowerShell 7 and cmd on PATH.
    fn windows_host() -> FakeHost {
        FakeHost::default()
            .on_path("pwsh.exe", r"C:\Program Files\PowerShell\7\pwsh.exe")
            .on_path("cmd.exe", r"C:\Windows\System32\cmd.exe")
    }

    #[test]
    fn the_env_override_wins_over_the_setting() {
        let host = windows_host().env("RUSTLING_TULIP_SHELL", "cmd.exe");
        let shell = host.resolve(Some(&setting("pwsh", None)), None);
        assert_eq!(shell.program, "cmd.exe");
        assert_eq!(shell.label, "cmd");
    }

    #[test]
    fn the_setting_wins_over_automatic() {
        let shell = windows_host().resolve(Some(&setting("cmd", None)), None);
        assert_eq!(shell.program, r"C:\Windows\System32\cmd.exe");
        assert_eq!(shell.label, "cmd");
        assert_eq!(shell.args.first().map(String::as_str), Some("/K"));

        let shell = windows_host().resolve(None, None);
        assert_eq!(shell.program, "pwsh.exe", "with no setting, Automatic");
        assert_eq!(shell.label, "pwsh");
    }

    #[test]
    fn an_unavailable_or_unknown_choice_falls_back_to_automatic() {
        for id in ["nu", "git-bash", "wsl", "tcsh", "auto"] {
            let shell = windows_host().resolve(Some(&setting(id, None)), None);
            assert_eq!(shell.program, "pwsh.exe", "{id} falls back to Automatic");
            assert_eq!(shell.label, "pwsh");
        }
    }

    #[test]
    fn a_custom_command_with_arguments_runs_as_given() {
        let shell = windows_host()
            .file(PathBuf::from("C:/Program Files/Git/bin/bash.exe"))
            .resolve(
                Some(&setting(
                    "custom",
                    Some("\"C:/Program Files/Git/bin/bash.exe\" -l -i"),
                )),
                None,
            );
        assert_eq!(shell.program, "C:/Program Files/Git/bin/bash.exe");
        assert_eq!(shell.args, ["-l", "-i"]);
        assert_eq!(shell.label, "bash");
        assert!(shell.extra_env.is_empty(), "no integration is added");
    }

    #[test]
    fn a_custom_command_without_arguments_gets_its_stems_integration() {
        let shell = windows_host()
            .on_path("pwsh", r"C:\Program Files\PowerShell\7\pwsh.exe")
            .resolve(Some(&setting("custom", Some("pwsh"))), None);
        assert_eq!(
            shell.program, r"C:\Program Files\PowerShell\7\pwsh.exe",
            "a bare name runs what PATH resolves it to"
        );
        assert_eq!(shell.label, "pwsh");
        assert_eq!(
            shell
                .args
                .iter()
                .take(2)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["-NoExit", "-Command"],
            "pwsh's prompt integration"
        );
    }

    #[test]
    fn a_custom_command_that_does_not_split_falls_back_to_automatic() {
        for command in [Some("\"pwsh"), Some("   "), Some(""), None] {
            let shell = windows_host().resolve(Some(&setting("custom", command)), None);
            assert_eq!(shell.program, "pwsh.exe", "{command:?} falls back");
        }
    }

    #[test]
    fn wsl_starts_in_the_session_folder() {
        let host = windows_host()
            .env("SystemRoot", r"C:\Windows")
            .file(wsl_path());
        let shell = host.resolve(
            Some(&setting("wsl", None)),
            Some(Path::new(r"C:\work\repo")),
        );
        assert_eq!(shell.program, wsl_path().to_string_lossy());
        assert_eq!(shell.args, ["--cd", r"C:\work\repo"]);
        assert_eq!(shell.label, "wsl");
        assert_eq!(
            shell.extra_env,
            Vec::<(String, String)>::new(),
            "WSL gets no integration"
        );

        let shell = host.resolve(Some(&setting("wsl", None)), None);
        assert!(shell.args.is_empty(), "no folder, no --cd");
    }

    fn split(host: &FakeHost, os: HostOs, command: &str) -> Option<Vec<String>> {
        host.probe(|probe| split_custom_command(os, command, probe))
    }

    #[test]
    fn a_windows_custom_command_keeps_its_backslashes() {
        let host = FakeHost::default();
        assert_eq!(
            split(&host, HostOs::Windows, r"C:\Tools\pwsh.exe -NoLogo"),
            Some(vec![r"C:\Tools\pwsh.exe".to_owned(), "-NoLogo".to_owned()])
        );
        assert_eq!(
            split(&host, HostOs::Windows, "'pwsh"),
            Some(vec!["'pwsh".to_owned()]),
            "a single quote is an ordinary character"
        );
    }

    #[test]
    fn a_windows_custom_command_naming_a_whole_path_is_one_word() {
        let pwsh = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let host = FakeHost::default().file(PathBuf::from(pwsh));
        for command in [pwsh.to_owned(), format!("  {pwsh} "), format!("\"{pwsh}\"")] {
            assert_eq!(
                split(&host, HostOs::Windows, &command),
                Some(vec![pwsh.to_owned()]),
                "{command:?} names the program"
            );
        }
        assert_eq!(
            split(&host, HostOs::Windows, &format!("\"{pwsh}\" -NoLogo")),
            Some(vec![pwsh.to_owned(), "-NoLogo".to_owned()]),
            "a quoted path with arguments splits"
        );
    }

    #[test]
    fn a_windows_custom_command_follows_the_command_line_rules_for_quotes() {
        let host = FakeHost::default();
        assert_eq!(
            split(&host, HostOs::Windows, r#"a\\"b c""#),
            Some(vec![r"a\b c".to_owned()]),
            "2n backslashes before a quote give n and toggle quoting"
        );
        assert_eq!(
            split(&host, HostOs::Windows, r#"a\"b"#),
            Some(vec![r#"a"b"#.to_owned()]),
            "2n+1 backslashes before a quote give n and a literal quote"
        );
        assert_eq!(
            split(&host, HostOs::Windows, r#"x "a b"  c\d"#),
            Some(vec!["x".to_owned(), "a b".to_owned(), r"c\d".to_owned()])
        );
        assert_eq!(
            split(&host, HostOs::Windows, r#""pwsh -NoLogo"#),
            None,
            "an unclosed double quote does not split"
        );
    }

    #[test]
    fn a_unix_custom_command_still_splits_by_shell_words() {
        let host = FakeHost::default();
        assert_eq!(
            split(&host, HostOs::Unix, r"'/opt/my shell/nu' --login a\ b"),
            Some(vec![
                "/opt/my shell/nu".to_owned(),
                "--login".to_owned(),
                "a b".to_owned()
            ])
        );
        assert_eq!(split(&host, HostOs::Unix, "'nu"), None);
    }

    #[test]
    fn a_windows_custom_path_with_arguments_resolves_as_given() {
        let shell = windows_host()
            .file(PathBuf::from(r"C:\Tools\pwsh.exe"))
            .resolve(
                Some(&setting("custom", Some(r"C:\Tools\pwsh.exe -NoLogo"))),
                None,
            );
        assert_eq!(shell.program, r"C:\Tools\pwsh.exe");
        assert_eq!(shell.args, ["-NoLogo"]);
    }

    #[test]
    fn a_custom_bare_name_that_resolves_to_wsl_bash_gets_no_family_and_no_integration() {
        let wsl_bash = r"C:\Windows\System32\bash.exe";
        let shell = windows_host()
            .on_path("bash", wsl_bash)
            .resolve(Some(&setting("custom", Some("bash"))), None);
        assert_eq!(shell.program, wsl_bash, "the program is the resolved path");
        assert_eq!(
            ShellFamily::of(&shell.program),
            None,
            "WSL's bash hosts no agent"
        );
        assert_eq!(shell.args, Vec::<String>::new(), "no --rcfile");
        assert_eq!(shell.extra_env, Vec::<(String, String)>::new());
    }

    #[test]
    fn a_custom_program_that_does_not_resolve_falls_back_to_automatic() {
        for command in [
            r"C:\Tools\nu.exe",
            r"C:\Tools\nu.exe -l",
            "nu",
            "\"\"",
            "\"\" -l",
        ] {
            let shell = windows_host().resolve(Some(&setting("custom", Some(command))), None);
            assert_eq!(shell.program, "pwsh.exe", "{command:?} falls back");
        }
    }

    #[test]
    fn a_resume_line_never_goes_into_a_shell_that_cannot_host_it() {
        let host = windows_host()
            .env("SystemRoot", r"C:\Windows")
            .file(wsl_path())
            .on_path("nu.exe", r"C:\Tools\nu.exe");
        let wsl = setting("wsl", None);
        let resuming = host.resolve_typing(Some(&wsl), None, true);
        assert_eq!(
            resuming.program, "pwsh.exe",
            "Automatic types the resume line"
        );
        assert_eq!(resuming.label, "pwsh");
        let plain = host.resolve_typing(Some(&wsl), None, false);
        assert!(
            plain.program.ends_with("wsl.exe"),
            "a plain shell keeps WSL: {}",
            plain.program
        );

        let nu = setting("nu", None);
        assert_eq!(
            host.resolve_typing(Some(&nu), None, true).program,
            r"C:\Tools\nu.exe",
            "Nushell types the resume line"
        );

        // Classifying cmd by its Windows path needs Windows path parsing.
        if cfg!(windows) {
            let cmd = setting("cmd", None);
            assert_eq!(
                host.resolve_typing(Some(&cmd), None, true).program,
                r"C:\Windows\System32\cmd.exe",
                "a shell with a family keeps the resume line"
            );
        }
    }

    #[test]
    fn nushell_runs_bare() {
        let host = windows_host().on_path("nu.exe", r"C:\Tools\nu.exe");
        let shell = host.resolve(Some(&setting("nu", None)), None);
        assert_eq!(shell.program, r"C:\Tools\nu.exe");
        assert_eq!(shell.label, "nu");
        assert!(shell.args.is_empty() && shell.extra_env.is_empty());
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod bash_init_tests {
    use super::BASH_INIT_SCRIPT;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    /// The bash the daemon runs a plain bash shell with: Git Bash on
    /// Windows, `bash` on the PATH elsewhere.
    fn bash() -> PathBuf {
        if cfg!(windows) {
            let root = std::env::var_os("ProgramFiles").expect("ProgramFiles is set");
            PathBuf::from(root).join(r"Git\bin\bash.exe")
        } else {
            PathBuf::from("bash")
        }
    }

    /// The OSC 133 marks (`A`, `B`, `C`, `D`) and the `hi` the command prints,
    /// in the order an interactive bash wrote them to stdout running the init
    /// script over a `.bashrc` holding `bashrc`, with `input` typed.
    fn marks(bashrc: &str, input: &str) -> Vec<String> {
        let dir =
            std::env::temp_dir().join(format!("rt-bash-init-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("create the scratch home");
        std::fs::write(dir.join(".bashrc"), bashrc).expect("write .bashrc");
        let init = dir.join("bash-init.sh");
        std::fs::write(&init, BASH_INIT_SCRIPT).expect("write the init script");
        let mut child = Command::new(bash())
            .arg("--rcfile")
            .arg(&init)
            .arg("-i")
            .env("HOME", &dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("bash starts");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input.as_bytes())
            .expect("type the input");
        let out = child.wait_with_output().expect("bash exits");
        let _ = std::fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout);
        let pattern = regex::Regex::new(r"\x1b\]133;([ABCD])|(hi)\n").expect("pattern compiles");
        pattern
            .captures_iter(&text)
            .map(|caps| {
                caps.get(1)
                    .or_else(|| caps.get(2))
                    .map_or_else(String::new, |m| m.as_str().to_owned())
            })
            .collect()
    }

    /// The DEBUG trap fires for every `PROMPT_COMMAND` statement too: a user's
    /// own prompt command (starship's, for one) must not read as a command
    /// starting after the prompt mark, and the user's command must.
    #[test]
    fn bash_init_marks_the_users_command_and_not_their_prompt_command() {
        let bashrc = "HISTFILE=/dev/null\n__user_precmd() { :; }\nPROMPT_COMMAND='__user_precmd'\n";
        assert_eq!(
            marks(bashrc, "echo hi\nexit\n"),
            ["A", "C", "hi", "D", "A", "C"],
            "prompt, `echo hi` starting, its output, its end, prompt, `exit` starting"
        );
    }

    /// With no prompt command of the user's, the shell's start marks no
    /// command, and an empty line starts none.
    #[test]
    fn bash_init_marks_no_command_at_start_or_for_an_empty_line() {
        assert_eq!(
            marks("HISTFILE=/dev/null\n", "\necho hi\nexit\n"),
            ["A", "A", "C", "hi", "D", "A", "C"],
            "prompt, prompt again after Enter, `echo hi` starting, its output, its end, prompt, `exit` starting"
        );
    }
}
