//! Program resolution for spawns: the agent CLI (through npm shims on Windows) and the user's shell, with its prompt-integration scripts and init files.

use crate::paths::Dirs;
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
